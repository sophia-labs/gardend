//! Whiteboard tests. The pure ones read the shared fixture
//! (`tests/fixtures/whiteboard/whiteboard-fixture-v1.json`, the same file the
//! Shrubbery binding tests load); the store ones go through the REAL dispatch
//! table (the path a live `tools/call` takes) against a real graph in a temp
//! profile, as `flow_board_mcp`'s tests do. No mocks.
#![cfg(feature = "headless")]

use super::ops::{read_scene, scene_from_update_base64, scene_of_doc, write_dirty};
use super::outline::{arrows, outline_markdown, shapes};
use super::scene::{Element, Scene};
use crate::graph_service::{create_graph_service, CreateGraphInput};
use crate::local_jobs::LocalJobRegistry;
use crate::mcp_dispatch_registry::{lookup, McpCallCtx};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::panic::UnwindSafe;
use std::path::PathBuf;
use std::sync::Arc;
use uuid::Uuid;
use yrs::{Doc, Map as YMap, Transact};

const FIXTURE: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/fixtures/whiteboard/whiteboard-fixture-v1.json"
));

fn fixture_elements() -> Vec<Element> {
    let fixture: Value = serde_json::from_str(FIXTURE).expect("fixture parses");
    fixture["elements"]
        .as_array()
        .expect("elements")
        .iter()
        .map(|e| e.as_object().expect("element object").clone())
        .collect()
}

fn fixture_scene() -> Scene {
    let mut map = BTreeMap::new();
    for el in fixture_elements() {
        map.insert(el["id"].as_str().unwrap().to_string(), el);
    }
    Scene::from_elements(map)
}

/// The fixture written into a real Y.Doc the way the browser writes it: one
/// stringified element per key of the `elements` map.
fn fixture_doc() -> Doc {
    let doc = Doc::new();
    {
        let mut txn = doc.transact_mut();
        let elements = yrs::WriteTxn::get_or_insert_map(&mut txn, "elements");
        for el in fixture_elements() {
            let id = el["id"].as_str().unwrap().to_string();
            elements.insert(&mut txn, id, serde_json::to_string(&el).unwrap());
        }
    }
    doc
}

#[test]
fn the_fixture_round_trips_through_a_ydoc_unchanged() {
    let doc = fixture_doc();
    let scene = scene_of_doc(&doc);
    assert_eq!(
        scene.elements.len(),
        7,
        "six live elements and one tombstone"
    );
    for el in fixture_elements() {
        let id = el["id"].as_str().unwrap();
        assert_eq!(
            Value::Object(scene.elements[id].clone()),
            Value::Object(el.clone()),
            "element {id} survives the Y.Map round trip"
        );
    }
    // The read goes through a second transaction type too.
    let txn = doc.transact();
    assert_eq!(read_scene(&txn).elements.len(), 7);
}

#[test]
fn the_fixture_reads_as_two_shapes_one_arrow_and_hides_the_tombstone() {
    let scene = fixture_scene();
    let shapes = shapes(&scene);
    let kinds: Vec<(&str, &str)> = shapes.iter().map(|s| (s.kind, s.text.as_str())).collect();
    assert_eq!(
        kinds,
        vec![("box", "Freud: the dream-work"), ("note", "condensation")],
        "bound text is folded into its container and not listed; the deleted box is hidden"
    );
    assert_eq!(
        shapes[1].color, "yellow",
        "a filled rectangle is a note; its colour is a name"
    );
    assert_eq!(shapes[0].by.as_deref(), Some("person"));
    let arrows = arrows(&scene);
    assert_eq!(arrows.len(), 1);
    assert_eq!(arrows[0].from.as_deref(), Some("box0000000000000000001"));
    assert_eq!(arrows[0].to.as_deref(), Some("note000000000000000001"));
    assert_eq!(arrows[0].label.as_deref(), Some("condenses"));
    assert_eq!(
        outline_markdown("Dreams", &scene),
        "# Dreams\n\n\
         - Freud: the dream-work (box)\n  - → condensation (via \"condenses\")\n\
         - condensation (note)\n"
    );
    assert_eq!(
        outline_markdown("Dreams", &scene),
        outline_markdown("Dreams", &fixture_scene()),
        "the same board gives the same bytes"
    );
}

// ------------------------------------------------------------ the real store

fn with_profile(prefix: &str, test: impl FnOnce() + UnwindSafe) {
    let _serial = crate::tauri_runtime::profile_env_serial()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let profile: PathBuf = std::env::temp_dir().join(format!("{prefix}-{}", Uuid::new_v4()));
    std::env::set_var("GARDEN_PROFILE_DIR", &profile);
    let result = std::panic::catch_unwind(test);
    std::env::remove_var("GARDEN_PROFILE_DIR");
    let _ = std::fs::remove_dir_all(&profile);
    if let Err(payload) = result {
        std::panic::resume_unwind(payload);
    }
}

fn call(ctx: &McpCallCtx<'_>, name: &str, args: Value) -> Result<Value, String> {
    let entry = lookup(name).unwrap_or_else(|| panic!("dispatch entry for {name}"));
    crate::app_runtime::async_runtime::block_on((entry.handler)(ctx, &args))
        .map_err(|error| error.to_string())
}

struct Harness {
    app: crate::app_runtime::AppHandle,
    jobs: Arc<LocalJobRegistry>,
    graph_id: String,
    jobs_dir: PathBuf,
}

impl Harness {
    fn new(graph_id: &str) -> Self {
        let app = crate::tauri_runtime::build_mock_app_for_tests(true);
        create_graph_service(
            &app,
            CreateGraphInput {
                title: format!("whiteboard {graph_id}"),
                graph_id: Some(graph_id.to_string()),
                description: None,
                operation_id: None,
            },
        )
        .expect("create graph");
        let jobs_dir = std::env::temp_dir().join(format!("wb-jobs-{}", Uuid::new_v4()));
        let jobs = Arc::new(LocalJobRegistry::new(jobs_dir.clone()).expect("job registry"));
        Self {
            app,
            jobs,
            graph_id: graph_id.to_string(),
            jobs_dir,
        }
    }
    fn call(&self, name: &str, mut args: Value) -> Result<Value, String> {
        args["graphId"] = json!(self.graph_id);
        let ctx = McpCallCtx {
            app: self.app.clone(),
            jobs: &self.jobs,
        };
        call(&ctx, name, args)
    }
    fn create(&self, title: &str) -> String {
        self.call("whiteboard_create", json!({ "title": title }))
            .expect("create whiteboard")["documentId"]
            .as_str()
            .unwrap()
            .to_string()
    }
    fn apply(&self, doc: &str, moves: Value) -> Result<Value, String> {
        self.call(
            "whiteboard_apply",
            json!({ "documentId": doc, "moves": moves }),
        )
    }
    fn read(&self, doc: &str) -> Value {
        self.call("whiteboard_read", json!({ "documentId": doc }))
            .expect("read")
    }
    fn elements(&self, doc: &str) -> Value {
        self.call(
            "whiteboard_read",
            json!({ "documentId": doc, "format": "elements" }),
        )
        .expect("read elements")
    }
}

impl Drop for Harness {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.jobs_dir);
    }
}

fn shape_by_text<'a>(outline: &'a Value, text: &str) -> &'a Value {
    outline["shapes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["text"] == text)
        .unwrap_or_else(|| panic!("no shape '{text}' in {outline}"))
}

fn element<'a>(elements: &'a Value, id: &str) -> &'a Value {
    elements["elements"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["id"] == id)
        .unwrap_or_else(|| panic!("no element {id}"))
}

#[test]
fn create_apply_read_the_whole_move_grammar() {
    with_profile("wb-grammar", || {
        let h = Harness::new("wb-grammar");
        let doc = h.create("Dreams");

        let empty = h.read(&doc);
        assert_eq!(empty["shapes"], json!([]));
        assert_eq!(empty["elementCount"], 0);
        assert!(empty["markdown"]
            .as_str()
            .unwrap()
            .contains("(empty board)"));

        // add + add near + connect, with refs, in one call.
        let out = h
            .apply(
                &doc,
                json!([
                    {"op":"add","ref":"work","shape":"box","text":"Freud: the dream-work"},
                    {"op":"add","ref":"cond","shape":"note","text":"condensation","near":{"ref":"work","side":"right"}},
                    {"op":"connect","ref":"edge","from":"work","to":"cond","label":"condenses"},
                    {"op":"add","ref":"disp","shape":"note","text":"displacement","near":{"ref":"cond","side":"below"}},
                    {"op":"connect","from":"work","to":"disp"}
                ]),
            )
            .expect("apply");
        let created = out["created"].as_object().unwrap();
        assert_eq!(
            created.len(),
            5,
            "every ref (and the unnamed arrow) reported: {created:?}"
        );
        let (work, cond, disp, edge) = (
            created["work"].as_str().unwrap().to_string(),
            created["cond"].as_str().unwrap().to_string(),
            created["disp"].as_str().unwrap().to_string(),
            created["edge"].as_str().unwrap().to_string(),
        );
        assert_eq!(work.len(), 21, "ids look like Excalidraw's");

        let r = h.read(&doc);
        assert_eq!(r["shapes"].as_array().unwrap().len(), 3);
        let (w, c, d) = (
            shape_by_text(&r, "Freud: the dream-work"),
            shape_by_text(&r, "condensation"),
            shape_by_text(&r, "displacement"),
        );
        assert_eq!(c["kind"], "note");
        assert_eq!(w["kind"], "box");
        assert!(
            c["x"].as_i64().unwrap() > w["x"].as_i64().unwrap() + w["w"].as_i64().unwrap(),
            "right of the box"
        );
        assert!(
            d["y"].as_i64().unwrap() > c["y"].as_i64().unwrap() + c["h"].as_i64().unwrap(),
            "below the note"
        );
        assert_eq!(w["by"], "tool", "attribution: no request actor in a test");
        assert_eq!(r["arrows"].as_array().unwrap().len(), 2);
        let arrow = r["arrows"]
            .as_array()
            .unwrap()
            .iter()
            .find(|a| a["id"] == edge)
            .unwrap();
        assert_eq!(
            (arrow["from"].as_str(), arrow["to"].as_str()),
            (Some(work.as_str()), Some(cond.as_str()))
        );
        assert_eq!(arrow["label"], "condenses");

        // Bound text and bound arrow wiring, on the raw elements.
        let raw = h.elements(&doc);
        let work_el = element(&raw, &work);
        let bound = work_el["boundElements"].as_array().unwrap();
        let text_id = bound.iter().find(|b| b["type"] == "text").unwrap()["id"]
            .as_str()
            .unwrap();
        let text_el = element(&raw, text_id);
        assert_eq!(text_el["containerId"], work);
        assert_eq!(
            text_el["originalText"], "Freud: the dream-work",
            "originalText is what was typed; text is wrapped at the box"
        );
        assert_eq!(
            bound.iter().filter(|b| b["type"] == "arrow").count(),
            2,
            "both arrows are listed on the box"
        );
        let arrow_el = element(&raw, &edge);
        assert_eq!(arrow_el["startBinding"]["elementId"], work);
        assert_eq!(arrow_el["endBinding"]["elementId"], cond);
        assert_eq!(arrow_el["endBinding"]["mode"], "orbit");
        assert!(element(&raw, &cond)["boundElements"]
            .as_array()
            .unwrap()
            .iter()
            .any(|b| b["id"] == edge));
        assert_ne!(
            arrow_el["x"], 0,
            "an arrow is routed, not left at the origin"
        );
        assert!(arrow_el["points"][1][0].as_f64().unwrap() > 0.0);
        assert_eq!(work_el["customData"]["sophia"]["by"], "tool");
        let (version, nonce) = (
            work_el["version"].as_i64().unwrap(),
            work_el["versionNonce"].as_i64().unwrap(),
        );
        assert!(version >= 1 && nonce > 0);

        // update: text and colour.
        h.apply(
            &doc,
            json!([{"op":"update","id":cond,"text":"condensation (Verdichtung)","color":"green"}]),
        )
        .unwrap();
        let r = h.read(&doc);
        let c = shape_by_text(&r, "condensation (Verdichtung)");
        assert_eq!(c["color"], "green");
        assert_eq!(c["kind"], "note");
        let raw = h.elements(&doc);
        assert!(
            element(&raw, &cond)["version"].as_i64().unwrap() > 1,
            "a change bumps the version"
        );

        // move: the arrows follow.
        let before = element(&h.elements(&doc), &edge).clone();
        h.apply(
            &doc,
            json!([{"op":"move","id":disp,"by":{"dx":400,"dy":300}}]),
        )
        .unwrap();
        let raw = h.elements(&doc);
        let arrow_to_disp = r["arrows"]
            .as_array()
            .unwrap()
            .iter()
            .find(|a| a["to"] == disp)
            .unwrap()["id"]
            .as_str()
            .unwrap()
            .to_string();
        let after = element(&raw, &arrow_to_disp);
        let end_x = after["x"].as_f64().unwrap() + after["points"][1][0].as_f64().unwrap();
        let end_y = after["y"].as_f64().unwrap() + after["points"][1][1].as_f64().unwrap();
        let moved = shape_by_text(&h.read(&doc), "displacement").clone();
        let (mx, my, mw, mh) = (
            moved["x"].as_f64().unwrap(),
            moved["y"].as_f64().unwrap(),
            moved["w"].as_f64().unwrap(),
            moved["h"].as_f64().unwrap(),
        );
        assert!(
            end_x >= mx - 8.0
                && end_x <= mx + mw + 8.0
                && end_y >= my - 8.0
                && end_y <= my + mh + 8.0,
            "the arrow now ends at the moved note: ({end_x},{end_y}) vs ({mx},{my},{mw},{mh})"
        );
        assert_eq!(
            element(&raw, &edge)["points"],
            before["points"],
            "an arrow that touches neither moved shape is left alone"
        );
        let moved_text = element(&raw, &raw_text_of(&raw, &disp));
        assert!(
            moved_text["x"].as_f64().unwrap() > mx - 1.0
                && moved_text["x"].as_f64().unwrap() < mx + mw,
            "the label went with the note"
        );

        // link: a document of this graph; the outline names its title.
        let other = h.create("Source text");
        h.apply(&doc, json!([{"op":"link","id":work,"documentId":other}]))
            .unwrap();
        let r = h.read(&doc);
        let w = shape_by_text(&r, "Freud: the dream-work");
        assert_eq!(
            w["link"],
            format!("mnemosyne://document/wb-grammar/{other}")
        );
        assert_eq!(w["linkTitle"], "Source text");
        assert!(r["markdown"]
            .as_str()
            .unwrap()
            .contains("[link: Source text]"));
        let err = h
            .apply(&doc, json!([{"op":"link","id":work,"documentId":"nope"}]))
            .expect_err("unknown document");
        assert!(err.contains("not in this graph"), "{err}");

        // group with a frame.
        h.apply(
            &doc,
            json!([{"op":"group","ids":[work, cond],"frame":"Mechanisms"}]),
        )
        .unwrap();
        let r = h.read(&doc);
        let frame = r["shapes"]
            .as_array()
            .unwrap()
            .iter()
            .find(|s| s["kind"] == "frame")
            .unwrap();
        assert_eq!(frame["text"], "Mechanisms");
        assert_eq!(
            shape_by_text(&r, "Freud: the dream-work")["frame"],
            frame["id"]
        );
        let md = r["markdown"].as_str().unwrap();
        assert!(md.contains("## Frame: Mechanisms"), "{md}");
        assert!(
            md.find("## Frame").unwrap() < md.find("## Elsewhere").unwrap(),
            "frames first: {md}"
        );

        // delete: the shape, its label and its arrows go (tombstones).
        let deleted = h.apply(&doc, json!([{"op":"delete","id":disp}])).unwrap();
        assert_eq!(
            deleted["arrows"].as_array().unwrap().len(),
            1,
            "the arrow to the deleted note went with it"
        );
        assert!(deleted["shapes"]
            .as_array()
            .unwrap()
            .iter()
            .all(|s| s["text"] != "displacement"));
        let raw = h.elements(&doc);
        assert!(
            raw["deletedCount"].as_i64().unwrap() >= 3,
            "note, its label, arrow: tombstoned, not erased"
        );
        assert_eq!(element(&raw, &disp)["isDeleted"], true);
        assert!(!element(&raw, &work)["boundElements"]
            .as_array()
            .unwrap()
            .iter()
            .any(|b| b["type"] == "arrow" && b["id"] == arrow_to_disp));
    });
}

fn raw_text_of(raw: &Value, container: &str) -> String {
    element(raw, container)["boundElements"]
        .as_array()
        .unwrap()
        .iter()
        .find(|b| b["type"] == "text")
        .unwrap()["id"]
        .as_str()
        .unwrap()
        .to_string()
}

#[test]
fn a_new_shape_with_no_position_lands_in_free_space_and_near_avoids_overlap() {
    with_profile("wb-place", || {
        let h = Harness::new("wb-place");
        let doc = h.create("Placement");
        let out = h
            .apply(
                &doc,
                json!([
                    {"op":"add","ref":"a","shape":"box","text":"first"},
                    {"op":"add","ref":"b","shape":"box","text":"second"},
                    {"op":"add","ref":"c","shape":"box","text":"third","near":{"ref":"a","side":"right"}},
                    {"op":"add","ref":"d","shape":"box","text":"fourth","near":{"ref":"a","side":"right"}}
                ]),
            )
            .unwrap();
        let rects: Vec<(i64, i64, i64, i64)> = ["first", "second", "third", "fourth"]
            .iter()
            .map(|t| {
                let s = shape_by_text(&out, t);
                (
                    s["x"].as_i64().unwrap(),
                    s["y"].as_i64().unwrap(),
                    s["w"].as_i64().unwrap(),
                    s["h"].as_i64().unwrap(),
                )
            })
            .collect();
        assert_eq!(
            rects[0].0, 0,
            "the first shape on an empty board is at the origin"
        );
        assert!(
            rects[1].0 >= rects[0].0 + rects[0].2,
            "the second goes to the right of everything"
        );
        for i in 0..4 {
            for j in (i + 1)..4 {
                let (a, b) = (rects[i], rects[j]);
                let overlap =
                    a.0 < b.0 + b.2 && b.0 < a.0 + a.2 && a.1 < b.1 + b.3 && b.1 < a.1 + a.3;
                assert!(!overlap, "shapes {i} and {j} overlap: {a:?} {b:?}");
            }
        }
    });
}

#[test]
fn a_malformed_move_refuses_the_whole_call_and_writes_nothing() {
    with_profile("wb-refuse", || {
        let h = Harness::new("wb-refuse");
        let doc = h.create("Refusals");
        h.apply(
            &doc,
            json!([{"op":"add","ref":"a","shape":"box","text":"kept"}]),
        )
        .unwrap();
        let before = h.elements(&doc);
        let bad_calls = [
            json!([{"op":"add","shape":"box","text":"good"},{"op":"explode"}]),
            json!([{"op":"add","shape":"box","text":"good"},{"op":"add","shape":"hexagon"}]),
            json!([{"op":"add","shape":"box","text":"good"},{"op":"connect","from":"nope","to":"nope2"}]),
            json!([{"op":"add","shape":"box","text":"good"},{"op":"add","shape":"note","color":"mauve"}]),
            json!([{"op":"add","shape":"box","text":"good"},{"op":"add","shape":"box","extra":1}]),
            json!([{"op":"add","shape":"box","text":"good"},{"op":"move","id":"x","by":{"dx":1,"dy":1},"at":{"x":1,"y":1}}]),
            json!([{"op":"add","shape":"box","text":"good"},{"op":"add","shape":"box","near":{"id":"missing","side":"left"}}]),
        ];
        for bad in bad_calls {
            let err = h
                .apply(&doc, bad.clone())
                .expect_err(&format!("{bad} must be refused"));
            assert!(err.contains("move 2"), "the error names the move: {err}");
            let after = h.elements(&doc);
            assert_eq!(
                after["elements"], before["elements"],
                "nothing written by {bad}"
            );
            assert_eq!(after["revision"], before["revision"]);
        }
        let err = h.apply(&doc, json!([])).expect_err("empty");
        assert!(err.contains("moves"), "{err}");
        let err = h
            .call("whiteboard_read", json!({"documentId":"no-such-doc"}))
            .expect_err("unknown doc");
        assert!(err.contains("not a whiteboard"), "{err}");
    });
}

#[test]
fn flush_saves_kind_bytes_and_the_outline_and_search_finds_the_words() {
    with_profile("wb-flush", || {
        let h = Harness::new("wb-flush");
        let doc = h.create("Dream atlas");
        h.apply(
            &doc,
            json!([
                {"op":"add","ref":"w","shape":"box","text":"Freud: the dream-work"},
                {"op":"add","ref":"d","shape":"note","text":"displacement","near":{"ref":"w","side":"below"}},
                {"op":"connect","from":"w","to":"d"}
            ]),
        )
        .unwrap();
        let graph_dir = crate::graph_paths::existing_graph_dir(&h.app, &h.graph_id).unwrap();
        let record: Value = serde_json::from_slice(
            &std::fs::read(graph_dir.join("documents").join(&doc).join("document.json"))
                .expect("record"),
        )
        .unwrap();
        assert_eq!(
            record["documentKind"], "whiteboard",
            "the kind flag is on the record"
        );
        // One block per live shape (the two shapes), none for the arrow or the bound text.
        let block_texts: Vec<&str> = record["blocks"]
            .as_array()
            .unwrap()
            .iter()
            .map(|b| b["content"].as_str().unwrap())
            .collect();
        assert_eq!(block_texts, vec!["Freud: the dream-work", "displacement"]);
        assert_eq!(record["tiptapXml"], "");
        let outline = h.read(&doc)["markdown"].as_str().unwrap().to_string();
        assert_eq!(record["body"], outline.as_str(), "body is the outline");
        assert!(outline.contains("displacement (note)"), "{outline}");

        // The record's Y.Doc bytes ARE the board (a cold read after the room closes).
        let cold =
            scene_from_update_base64(record["ydocUpdateBase64"].as_str().unwrap_or("")).unwrap();
        let cold = if cold.elements.is_empty() {
            // The inline copy is the sidecar's; read the sidecar when the record omits it.
            let path = crate::ydoc_paths::document_ydoc_state_path(&graph_dir, &doc);
            super::ops::scene_from_update_bytes(&std::fs::read(path).unwrap()).unwrap()
        } else {
            cold
        };
        assert_eq!(cold.live().count(), 5, "two shapes with labels, an arrow");
        assert_eq!(outline_markdown("Dream atlas", &cold), outline);

        // read_document returns the outline, not an empty body.
        for format in ["markdown", "xml"] {
            let read = h
                .call(
                    "read_document",
                    json!({"documentId": doc, "format": format, "maxChars": 100000}),
                )
                .unwrap();
            assert!(
                read["content"].as_str().unwrap().contains("displacement"),
                "{format}: {read}"
            );
        }
        // search_documents finds the board by a word on it; search_blocks does too.
        let hits = h
            .call("search_documents", json!({"query": "displacement"}))
            .unwrap();
        assert!(
            hits["results"]
                .as_array()
                .unwrap()
                .iter()
                .any(|r| r["documentId"] == doc),
            "{hits}"
        );
        let blocks = h
            .call(
                "search_blocks",
                json!({"query": "displacement", "mode": "lexical"}),
            )
            .unwrap();
        assert!(
            blocks["results"].as_array().map_or(false, |r| r
                .iter()
                .any(|b| b["documentId"] == doc || b["document_id"] == doc)),
            "{blocks}"
        );
        // The workspace lists the board with its kind.
        // (the sidebar reads the kind with this SPARQL; get_workspace does not carry it).
        let kinds = h
            .call(
                "sparql_query",
                json!({"query": "PREFIX doc: <http://mnemosyne.dev/doc#> SELECT ?doc ?kind WHERE { GRAPH ?g { ?doc doc:documentKind ?kind } }"}),
            )
            .unwrap();
        assert!(kinds.to_string().contains("whiteboard"), "{kinds}");
    });
}

#[test]
fn a_person_edit_in_the_room_flushes_and_is_seen_by_the_next_read() {
    with_profile("wb-person", || {
        let h = Harness::new("wb-person");
        let doc = h.create("Shared");
        // Drive the live room as a browser's y-websocket client would: write one
        // stringified element into the `elements` map of the room's Y.Doc.
        let registry = h.app.state::<crate::crdt_engine::rooms::RoomRegistry>();
        let graph_dir = crate::graph_paths::existing_graph_dir(&h.app, &h.graph_id).unwrap();
        let room = crate::app_runtime::async_runtime::block_on(registry.get_or_create(
            &format!("doc:{}:{doc}", h.graph_id),
            crate::ydoc_paths::document_ydoc_state_path(&graph_dir, &doc),
        ))
        .unwrap();
        let mut person = fixture_elements().remove(0);
        person.insert("id".into(), json!("person0000000000000001"));
        person.insert("boundElements".into(), json!([]));
        let mut scene = Scene::default();
        scene
            .elements
            .insert("person0000000000000001".into(), person);
        scene.dirty.insert("person0000000000000001".into());
        crate::app_runtime::async_runtime::block_on(room.update_doc(|_d, txn| {
            write_dirty(txn, &scene);
            Ok(())
        }))
        .unwrap();
        let r = h.read(&doc);
        assert_eq!(
            r["shapes"].as_array().unwrap().len(),
            1,
            "the tool sees a person's shape: {r}"
        );
        assert_eq!(r["shapes"][0]["by"], "person");
        // And the tool can move it (a person's element is edited in place).
        h.apply(
            &doc,
            json!([{"op":"move","id":"person0000000000000001","at":{"x":900,"y":50}}]),
        )
        .unwrap();
        let moved = &h.read(&doc)["shapes"][0];
        assert_eq!(
            (moved["x"].as_i64(), moved["y"].as_i64()),
            (Some(900), Some(50))
        );
        assert_eq!(moved["by"], "tool", "the last writer is the one recorded");
    });
}

#[test]
fn restore_keeps_the_kind_and_the_board_and_its_outline() {
    with_profile("wb-restore", || {
        let h = Harness::new("wb-restore");
        let doc = h.create("Restorable");
        h.apply(
            &doc,
            json!([{"op":"add","shape":"note","text":"before the restore point"}]),
        )
        .unwrap();
        let point = h
            .call("create_restore_point", json!({"label": "wb"}))
            .unwrap();
        let rp = point["restorePointId"].as_str().unwrap().to_string();
        h.apply(
            &doc,
            json!([{"op":"add","shape":"note","text":"after the restore point"}]),
        )
        .unwrap();
        assert!(h.read(&doc)["markdown"]
            .as_str()
            .unwrap()
            .contains("after the restore point"));

        let graph_dir = crate::graph_paths::existing_graph_dir(&h.app, &h.graph_id).unwrap();
        crate::time_travel_restore_service::start_restore(
            &h.app,
            h.jobs.clone(),
            &h.graph_id,
            &rp,
            false,
        )
        .expect("start restore");
        // Wait for the restore job to finish.
        let record_path = graph_dir.join("documents").join(&doc).join("document.json");
        let mut body = String::new();
        for _ in 0..200 {
            let record: Value =
                serde_json::from_slice(&std::fs::read(&record_path).unwrap()).unwrap();
            body = record["body"].as_str().unwrap_or("").to_string();
            if !body.contains("after the restore point")
                && body.contains("before the restore point")
            {
                assert_eq!(
                    record["documentKind"], "whiteboard",
                    "restore keeps the kind"
                );
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        assert!(
            body.contains("before the restore point") && !body.contains("after the restore point"),
            "{body}"
        );
    });
}

#[test]
fn the_whiteboard_tools_are_registered_with_scopes_in_every_source_of_truth() {
    for name in ["whiteboard_create", "whiteboard_read", "whiteboard_apply"] {
        assert!(lookup(name).is_some(), "dispatch row for {name}");
        assert!(
            crate::mcp_tool_registry::is_known_mcp_tool_name(name),
            "{name} in the catalog"
        );
    }
    assert_eq!(
        crate::loopback_scopes::mcp_tool_scopes("whiteboard_create"),
        vec!["documents.write.crdt"]
    );
    assert_eq!(
        crate::loopback_scopes::mcp_tool_scopes("whiteboard_apply"),
        vec!["documents.write.crdt"]
    );
    assert_eq!(
        crate::loopback_scopes::mcp_tool_scopes("whiteboard_read"),
        vec!["documents.read"]
    );
}

#[test]
fn id_and_index_generation_are_deterministic_for_a_seed() {
    let mut a = super::scene::Rng(42);
    let mut b = super::scene::Rng(42);
    assert_eq!(a.id(), b.id(), "the same seed replays to the same ids");
    assert_ne!(a.id(), a.id());
}

#[test]
fn agent_drawn_elements_carry_the_sophia_look_and_a_label_gets_room() {
    with_profile("wb-look", || {
        let h = Harness::new("wb-look");
        let doc = h.create("Look");
        let out = h
            .apply(
                &doc,
                json!([
                    {"op":"add","ref":"a","shape":"box","text":"Freud: the dream-work","color":"green"},
                    {"op":"add","ref":"b","shape":"note","text":"condensation","color":"yellow","near":{"ref":"a","side":"right"}},
                    {"op":"connect","ref":"e","from":"a","to":"b","label":"condenses into a single image"}
                ]),
            )
            .unwrap();
        let raw = h.elements(&doc);
        let (a, b, e) = (
            element(&raw, out["created"]["a"].as_str().unwrap()),
            element(&raw, out["created"]["b"].as_str().unwrap()),
            element(&raw, out["created"]["e"].as_str().unwrap()),
        );
        for el in [a, b, e] {
            assert_eq!(el["roughness"], 0, "architect, not hand-drawn: {el}");
            assert_eq!(el["strokeWidth"], 1, "thin: {el}");
        }
        assert_eq!(a["strokeColor"], "#326f4c", "green is the Sophia accent");
        assert_eq!(
            b["backgroundColor"], "#fef3c7",
            "a yellow note is Sophia amber"
        );
        assert_eq!(a["roundness"], json!({"type": 3}));
        assert_eq!(e["roundness"], Value::Null, "crisp arrows");
        assert_eq!(e["elbowed"], false);
        let label = raw["elements"]
            .as_array()
            .unwrap()
            .iter()
            .find(|t| t["containerId"] == e["id"])
            .expect("the arrow's label");
        assert_eq!(label["fontFamily"], 6);
        // The label clears both shapes: the second shape was pushed out to make room.
        let (lx, lw) = (
            label["x"].as_f64().unwrap(),
            label["width"].as_f64().unwrap(),
        );
        let a_right = a["x"].as_f64().unwrap() + a["width"].as_f64().unwrap();
        let b_left = b["x"].as_f64().unwrap();
        assert!(
            lx > a_right + 8.0,
            "label starts clear of the first shape: {lx} vs {a_right}"
        );
        assert!(
            lx + lw < b_left - 8.0,
            "label ends clear of the second shape: {} vs {b_left}",
            lx + lw
        );
        // Names read back, old boards and new.
        let read = h.read(&doc);
        assert_eq!(
            shape_by_text(&read, "Freud: the dream-work")["color"],
            "green"
        );
    });
}

// ============================================================ G5: cards, links to objects and wires

const FIXTURE_V2: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/fixtures/whiteboard/whiteboard-fixture-v2-semantic.json"
));

fn v2_scene() -> Scene {
    let fixture: Value = serde_json::from_str(FIXTURE_V2).expect("v2 fixture parses");
    let mut map = BTreeMap::new();
    for el in fixture["elements"].as_array().expect("elements") {
        let el = el.as_object().expect("element object").clone();
        map.insert(el["id"].as_str().unwrap().to_string(), el);
    }
    Scene::from_elements(map)
}

const BOX: &str = "box0000000000000000001";
const NOTE: &str = "note000000000000000001";
const LINKED: &str = "lnk0000000000000000001";
const CARD: &str = "crd0000000000000000001";
const WIRED: &str = "arw0000000000000000002";
const WOLF: &str = "urn:mnemosyne:local:graph:wb-graph:dream:wolf-man";

fn pure_ctx<'a>(
    titles: &'a BTreeMap<String, String>,
    objects: &'a BTreeMap<String, super::scene::ObjectInfo>,
    seed: u64,
) -> super::scene::Ctx<'a> {
    super::scene::Ctx {
        actor: "tester",
        now: 1_760_000_009_000,
        rng: super::scene::Rng(seed),
        graph_id: "wb-graph",
        document_id: "wb-board",
        titles,
        objects,
        wires: Vec::new(),
        layouts: Box::leak(Box::new(BTreeMap::new())),
        pending_layout: None,
    }
}

#[test]
fn outline_reads_cards_and_wires() {
    let scene = v2_scene();
    let md = outline_markdown("Dream-work board", &scene);
    assert!(
        md.contains("- [card: DreamReport \u{2014} The Wolf Man dream]\n"),
        "{md}"
    );
    assert!(
        md.contains(
            "  - \u{2014}supports (wire)\u{2192} [card: DreamReport \u{2014} The Wolf Man dream]\n"
        ),
        "{md}"
    );
    assert!(
        md.contains("  - \u{2192} condensation (via \"condenses\")\n"),
        "an arrow that is not a wire reads as it always did: {md}"
    );

    let all = shapes(&scene);
    let ids: Vec<&str> = all.iter().map(|s| s.id.as_str()).collect();
    assert_eq!(ids, vec![BOX, NOTE, "frm0000000000000000001", CARD, LINKED]);
    let card = all.iter().find(|s| s.id == CARD).unwrap();
    assert_eq!(card.kind, "card");
    assert_eq!(
        card.text, "The Wolf Man dream",
        "a card's text is its label"
    );
    assert_eq!(
        card.card,
        Some(super::outline::CardRead {
            iri: Some(WOLF.to_string()),
            document_id: None,
            label: "The Wolf Man dream".into(),
            class_label: "DreamReport".into(),
        })
    );
    assert!(all
        .iter()
        .filter(|s| s.id != CARD)
        .all(|s| s.card.is_none()));
    let card_json = super::outline::shape_json(card);
    assert_eq!(
        card_json["card"],
        json!({"iri": WOLF, "label": "The Wolf Man dream", "classLabel": "DreamReport"})
    );
    assert_eq!(card_json["kind"], "card");

    let all_arrows = arrows(&scene);
    let wired = all_arrows.iter().find(|a| a.id == WIRED).unwrap();
    assert_eq!(
        wired.wire,
        Some(super::outline::WireRead {
            wire_id: "wire-wb-0001".into(),
            predicate: "supports".into()
        })
    );
    assert_eq!(
        super::outline::arrow_json(wired)["wire"],
        json!({"wireId": "wire-wb-0001", "predicate": "supports"})
    );
    assert!(all_arrows
        .iter()
        .filter(|a| a.id != WIRED)
        .all(|a| a.wire.is_none()));
    // The v1 board reads exactly as before: no card, no wire keys.
    let v1 = super::outline::arrow_json(&arrows(&fixture_scene())[0]);
    assert!(v1.get("wire").is_none());
}

#[test]
fn only_a_graph_link_makes_an_embeddable_a_card() {
    use super::scene::shape_kind;
    let mut el = v2_scene().elements[CARD].clone();
    assert_eq!(shape_kind(&el), "card");
    el.insert("link".into(), json!("https://example.com/video"));
    assert_eq!(shape_kind(&el), "box", "a web embeddable is a box");
    el.insert(
        "link".into(),
        json!(super::links::artifact_link("wb-graph", "art-1")),
    );
    assert_eq!(shape_kind(&el), "box", "an artifact is not a card");
    el.insert(
        "link".into(),
        json!(super::links::document_link("wb-graph", "doc-1")),
    );
    assert_eq!(shape_kind(&el), "card");
    el.insert("link".into(), Value::Null);
    assert_eq!(shape_kind(&el), "box");
}

#[test]
fn resolve_wire_end_follows_the_endpoint_rule() {
    use super::scene::resolve_wire_end;
    let scene = v2_scene();
    let (titles, objects) = (BTreeMap::new(), BTreeMap::new());
    let ctx = pure_ctx(&titles, &objects, 1);

    // A shape linked to a document of this graph is that document.
    let end = resolve_wire_end(&scene, &ctx, LINKED).unwrap();
    assert_eq!(end.document_id.as_deref(), Some("doc-traumdeutung"));
    assert_eq!((end.block_id, end.iri, end.graph_id), (None, None, None));
    // A card is its object's IRI, titled by the snapshot.
    let end = resolve_wire_end(&scene, &ctx, CARD).unwrap();
    assert_eq!(end.iri.as_deref(), Some(WOLF));
    assert_eq!(end.label.as_deref(), Some("The Wolf Man dream"));
    assert_eq!((end.document_id, end.block_id), (None, None));
    // Any other shape is itself, a block of the board; its bound text stands for it.
    for id in [BOX, "txt0000000000000000001"] {
        let end = resolve_wire_end(&scene, &ctx, id).unwrap();
        assert_eq!(end.document_id.as_deref(), Some("wb-board"));
        assert_eq!(end.block_id.as_deref(), Some(BOX));
        assert_eq!(end.iri, None);
    }
    // Another graph's document is not this graph's: the shape stays itself.
    let mut foreign = scene.clone();
    foreign.elements.get_mut(LINKED).unwrap().insert(
        "link".into(),
        json!(super::links::document_link("elsewhere", "doc-x")),
    );
    let end = resolve_wire_end(&foreign, &ctx, LINKED).unwrap();
    assert_eq!(end.document_id.as_deref(), Some("wb-board"));
    assert_eq!(end.block_id.as_deref(), Some(LINKED));
    // An arrow, a deleted element and a missing one are refused by name.
    assert!(resolve_wire_end(&scene, &ctx, WIRED)
        .unwrap_err()
        .contains(WIRED));
    assert!(resolve_wire_end(&scene, &ctx, "del0000000000000000001")
        .unwrap_err()
        .contains("was deleted"));
    assert!(resolve_wire_end(&scene, &ctx, "nope")
        .unwrap_err()
        .contains("nope"));
}

#[test]
fn wire_move_marks_the_arrow_and_drafts_a_wire() {
    let scene = v2_scene();
    let (titles, objects) = (BTreeMap::new(), BTreeMap::new());
    let moves = vec![
        json!({"op":"add","ref":"a","shape":"box","text":"Origin"}),
        json!({"op":"add","ref":"b","shape":"box","text":"Target","near":{"ref":"a","side":"right"}}),
        json!({"op":"connect","ref":"plain","from":"a","to":"b"}),
        json!({"op":"wire","arrow":"plain","predicate":"supports"}),
        // An arrow that already has a label keeps it.
        json!({"op":"wire","arrow":WIRED_LABELLED,"predicate":"relatedTo"}),
    ];
    let run = |seed: u64| {
        let mut scene = scene.clone();
        let mut ctx = pure_ctx(&titles, &objects, seed);
        let created = super::scene::apply_moves(&mut scene, &moves, &mut ctx).expect("apply");
        (scene, created, ctx.wires)
    };
    let (after, created, wires) = run(7);
    let plain = &created.iter().find(|(name, _)| name == "plain").unwrap().1;
    assert_eq!(wires.len(), 2);
    let draft = &wires[0];
    assert_eq!(draft.predicate, "supports");
    assert!(draft.wire_id.starts_with("wire-") && draft.wire_id.len() == 5 + 21);
    assert_eq!(draft.scene.graph_id, "wb-graph");
    assert_eq!(draft.scene.document_id, "wb-board");
    assert_eq!(&draft.scene.element_id, plain);
    assert_eq!(draft.scene.stable_key, format!("wb:wb-board:{plain}"));
    assert_eq!(draft.source.document_id.as_deref(), Some("wb-board"));
    assert_eq!(
        Some(&draft.scene.source_element_id),
        draft.source.block_id.as_ref(),
        "an unlinked shape is a block of the board named by its element id"
    );
    assert_eq!(
        draft.target.block_id.as_ref(),
        Some(&draft.scene.target_element_id)
    );

    // The mark, the accent stroke and the label the predicate gave.
    let arrow = &after.elements[plain];
    let mark = &arrow["customData"]["mnemosyneWire"];
    assert_eq!(mark["wireId"], draft.wire_id.as_str());
    assert_eq!(mark["predicate"], "supports");
    assert_eq!(mark["strokeWas"], "#1d1c19");
    assert_eq!(mark["labelFromPredicate"], true);
    assert_eq!(arrow["strokeColor"], "#326f4c");
    let label = super::scene::bound_text(&after, arrow).expect("the arrow got a label");
    assert_eq!(label["originalText"], "supports");

    // The already-labelled arrow keeps its label, and says it did not make it.
    let kept = &after.elements[WIRED_LABELLED];
    assert_eq!(
        kept["customData"]["mnemosyneWire"]["labelFromPredicate"],
        false
    );
    assert_eq!(
        kept["customData"]["mnemosyneWire"]["strokeWas"], "#1e1e1e",
        "the stroke the fixture arrow had (the legacy ink), remembered for unwiring"
    );
    let label = super::scene::bound_text(&after, kept).unwrap();
    assert_eq!(label["originalText"], "condenses");

    // The outline now reads both as wires.
    let md = outline_markdown("t", &after);
    assert!(
        md.contains("\u{2014}supports (wire)\u{2192} Target"),
        "{md}"
    );
    assert!(
        md.contains("\u{2014}condenses (wire)\u{2192} condensation"),
        "{md}"
    );

    // Same seed, same ids; another seed, another wire id.
    let (_, _, again) = run(7);
    assert_eq!(again, wires);
    let (_, _, other) = run(8);
    assert_ne!(other[0].wire_id, wires[0].wire_id);
}

/// The v1 arrow (box to note, labelled "condenses") the v2 fixture keeps.
const WIRED_LABELLED: &str = "arw0000000000000000001";

// ---- the stores: objects, cards, wires

fn board_room(h: &Harness, doc: &str) -> Arc<crate::crdt_engine::rooms::Room> {
    let registry = h.app.state::<crate::crdt_engine::rooms::RoomRegistry>();
    let graph_dir = crate::graph_paths::existing_graph_dir(&h.app, &h.graph_id).unwrap();
    crate::app_runtime::async_runtime::block_on(registry.get_or_create(
        &format!("doc:{}:{doc}", h.graph_id),
        crate::ydoc_paths::document_ydoc_state_path(&graph_dir, doc),
    ))
    .unwrap()
}

/// Rewrite one element the way a browser would, then persist the board.
fn patch_element(h: &Harness, doc: &str, id: &str, edit: impl FnOnce(&mut Element)) {
    let room = board_room(h, doc);
    crate::app_runtime::async_runtime::block_on(room.update_doc(|_doc, txn| {
        let mut live = read_scene(&*txn);
        let el = live.elements.get_mut(id).ok_or("no such element")?;
        edit(el);
        live.dirty.insert(id.to_string());
        write_dirty(txn, &live);
        Ok(())
    }))
    .expect("patch the element");
    crate::app_runtime::async_runtime::block_on(
        crate::crdt_engine::document_ops::persist_room_document(
            &h.app,
            &h.graph_id,
            doc,
            "Board",
            &room,
            &format!("op-wbs-g5-patch-{}", Uuid::new_v4().simple()),
        ),
    )
    .expect("persist the patch");
}

/// A typed object the graph knows: a `DreamReport` (defined with `propose_domain_ontology`) with a label, written
/// as plain triples in a graph of its own. Returns its IRI.
fn put_dream(h: &Harness) -> String {
    let defined = h
        .call(
            "propose_domain_ontology",
            json!({"classes": [{
                "name": "DreamReport", "label": "Dream report", "plural": "Dream reports",
                "fields": [{"key": "who", "label": "Who", "datatype": "string", "required": true}],
                "presentation": {"title": "who"}
            }], "rationale": "a card names a class"}),
        )
        .expect("define the class");
    let class_iri = defined["classCatalog"]
        .as_array()
        .and_then(|c| c.iter().find(|c| c["name"] == "DreamReport"))
        .and_then(|c| c["iri"].as_str())
        .unwrap_or_else(|| panic!("DreamReport in {defined}"))
        .to_string();
    let iri = format!("urn:mnemosyne:local:graph:{}:dream:wolf-man", h.graph_id);
    h.call(
        "sparql_update",
        json!({"update": format!(
            "INSERT DATA {{ GRAPH <urn:mnemosyne:local:graph:{g}:wbs-objects> {{ \
             <{iri}> a <{class_iri}> ; <http://www.w3.org/2000/01/rdf-schema#label> \"The Wolf Man dream\" . }} }}",
            g = h.graph_id
        )}),
    )
    .expect("write the object");
    iri
}

fn labelled_text(raw: &Value, container: &str) -> Option<String> {
    raw["elements"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["containerId"] == container && e["isDeleted"] == false)
        .map(|e| e["originalText"].as_str().unwrap_or("").to_string())
}

#[test]
fn card_move_makes_a_live_embeddable() {
    with_profile("wbs-g5-card", || {
        let h = Harness::new("wbs-g5-card");
        let iri = put_dream(&h);
        let doc = h.create("Cards");
        let source = h.create("Source text");
        let out = h
            .apply(
                &doc,
                json!([
                    {"op":"card","ref":"wolf","iri":iri},
                    {"op":"card","ref":"src","documentId":source,"near":{"ref":"wolf","side":"right"}}
                ]),
            )
            .expect("two cards");
        let wolf = out["created"]["wolf"].as_str().unwrap().to_string();
        let src = out["created"]["src"].as_str().unwrap().to_string();
        assert_eq!(out["wires"], json!([]), "no wire moves, an empty list");

        let raw = h.elements(&doc);
        let wolf_el = element(&raw, &wolf);
        assert_eq!(wolf_el["type"], "embeddable");
        assert_eq!(
            wolf_el["link"],
            super::links::object_link(&h.graph_id, &iri).as_str()
        );
        assert_eq!(
            wolf_el["customData"]["mnemosyne"],
            json!({"kind": "object", "graphId": h.graph_id, "iri": iri,
                   "label": "The Wolf Man dream", "classLabel": "Dream report"})
        );
        assert_eq!(wolf_el["customData"]["sophia"]["by"], "tool");
        assert_eq!(
            (wolf_el["width"].as_f64(), wolf_el["height"].as_f64()),
            (Some(320.0), Some(180.0))
        );
        assert_eq!(wolf_el["boundElements"], json!([]));
        let src_el = element(&raw, &src);
        assert_eq!(src_el["type"], "embeddable");
        assert_eq!(
            src_el["link"],
            super::links::document_link(&h.graph_id, &source).as_str()
        );
        assert_eq!(
            src_el["customData"]["mnemosyne"],
            json!({"kind": "document", "graphId": h.graph_id, "id": source, "title": "Source text"})
        );
        assert!(
            src_el["x"].as_f64().unwrap() > wolf_el["x"].as_f64().unwrap() + 320.0,
            "placed to the right of the first card"
        );
        assert_eq!(
            raw["elementCount"], 2,
            "a card is not a text container: nothing else was made"
        );

        // It reads back as a card.
        let read = h.read(&doc);
        let shapes_read = read["shapes"].as_array().unwrap();
        let wolf_shape = shapes_read
            .iter()
            .find(|s| s["id"] == wolf.as_str())
            .unwrap();
        assert_eq!(wolf_shape["kind"], "card");
        assert_eq!(wolf_shape["text"], "The Wolf Man dream");
        assert_eq!(
            wolf_shape["card"],
            json!({"iri": iri, "label": "The Wolf Man dream", "classLabel": "Dream report"})
        );
        let src_shape = shapes_read
            .iter()
            .find(|s| s["id"] == src.as_str())
            .unwrap();
        assert_eq!(src_shape["kind"], "card");
        assert_eq!(
            src_shape["card"],
            json!({"documentId": source, "label": "Source text", "classLabel": ""})
        );
        let md = read["markdown"].as_str().unwrap();
        assert!(
            md.contains("[card: Dream report \u{2014} The Wolf Man dream]"),
            "{md}"
        );
        assert!(md.contains("[card: Source text]"), "{md}");

        // Refusals write nothing.
        let before = h.elements(&doc);
        for (moves, needle) in [
            (json!([{"op":"card"}]), "exactly one of iri, documentId"),
            (
                json!([{"op":"card","iri":iri,"documentId":source}]),
                "exactly one of iri, documentId",
            ),
            (
                json!([{"op":"card","iri":"urn:example:nothing-here"}]),
                "object 'urn:example:nothing-here' is not in this graph",
            ),
            (
                json!([{"op":"card","documentId":"no-such-document"}]),
                "document 'no-such-document' is not in this graph",
            ),
            (
                json!([{"op":"update","id":wolf,"text":"words"}]),
                "a card has no text",
            ),
        ] {
            let error = h.apply(&doc, moves).expect_err(needle);
            assert!(error.contains(needle), "{needle}: {error}");
        }
        assert_eq!(h.elements(&doc)["elements"], before["elements"]);
    });
}

#[test]
fn link_to_an_object_keeps_its_labels() {
    with_profile("wbs-g5-link", || {
        let h = Harness::new("wbs-g5-link");
        let iri = put_dream(&h);
        let doc = h.create("Links");
        let source = h.create("Source text");
        let out = h
            .apply(
                &doc,
                json!([
                    {"op":"add","ref":"a","shape":"box","text":"Wolf"},
                    {"op":"add","ref":"b","shape":"box","text":"Book","near":{"ref":"a","side":"right"}}
                ]),
            )
            .unwrap();
        let (a, b) = (
            out["created"]["a"].as_str().unwrap().to_string(),
            out["created"]["b"].as_str().unwrap().to_string(),
        );
        h.apply(
            &doc,
            json!([{"op":"link","id":a,"iri":iri}, {"op":"link","id":b,"documentId":source}]),
        )
        .expect("link");
        let raw = h.elements(&doc);
        let a_el = element(&raw, &a);
        assert_eq!(
            a_el["link"],
            super::links::object_link(&h.graph_id, &iri).as_str()
        );
        assert_eq!(
            a_el["customData"]["mnemosyne"],
            json!({"kind": "object", "graphId": h.graph_id, "iri": iri,
                   "label": "The Wolf Man dream", "classLabel": "Dream report"}),
            "the snapshot keeps both labels"
        );
        let b_el = element(&raw, &b);
        assert_eq!(
            b_el["link"],
            super::links::document_link(&h.graph_id, &source).as_str()
        );
        assert_eq!(
            b_el["customData"]["mnemosyne"],
            json!({"kind": "document", "graphId": h.graph_id, "id": source, "title": "Source text"})
        );
        assert_eq!(
            shape_by_text(&h.read(&doc), "Wolf")["kind"],
            "box",
            "a linked rectangle stays a box; only an embeddable is a card"
        );

        // An object the graph does not know is refused, and the board is as it was.
        let before = h.elements(&doc);
        let error = h
            .apply(
                &doc,
                json!([
                    {"op":"update","id":b,"text":"Renamed"},
                    {"op":"link","id":b,"iri":"urn:example:nothing-here"}
                ]),
            )
            .expect_err("unknown object");
        assert!(
            error.contains("object 'urn:example:nothing-here' is not in this graph"),
            "{error}"
        );
        let error = h
            .apply(&doc, json!([{"op":"link","id":b,"iri":"not an iri"}]))
            .expect_err("not an iri");
        assert!(error.contains("not an absolute IRI"), "{error}");
        assert_eq!(h.elements(&doc)["elements"], before["elements"]);
        assert_eq!(h.elements(&doc)["revision"], before["revision"]);
    });
}

// needs G3: `scene_wire::create_scene_wires` stores the drafts (`get_wires` reads them back with the scene fields).
#[test]
fn wire_move_follows_the_endpoint_rule() {
    with_profile("wbs-g5-wire", || {
        let h = Harness::new("wbs-g5-wire");
        let iri = put_dream(&h);
        let doc = h.create("Wires");
        let source = h.create("Source text");
        let out = h
            .apply(
                &doc,
                json!([
                    {"op":"add","ref":"a","shape":"box","text":"Origin"},
                    {"op":"card","ref":"c","iri":iri,"near":{"ref":"a","side":"right"}},
                    {"op":"add","ref":"p","shape":"box","text":"Plain","near":{"ref":"a","side":"below"}},
                    {"op":"add","ref":"q","shape":"box","text":"Other","near":{"ref":"p","side":"right"}},
                    {"op":"link","id":"a","documentId":source},
                    {"op":"connect","ref":"e1","from":"a","to":"c"},
                    {"op":"connect","ref":"e2","from":"p","to":"q","label":"relates"}
                ]),
            )
            .expect("draw");
        let id = |name: &str| out["created"][name].as_str().unwrap().to_string();
        let (a, c, p, q, e1, e2) = (id("a"), id("c"), id("p"), id("q"), id("e1"), id("e2"));

        let out = h
            .apply(
                &doc,
                json!([
                    {"op":"wire","arrow":e1,"predicate":"supports"},
                    {"op":"wire","arrow":e2,"predicate":"relatedTo"}
                ]),
            )
            .expect("wire");
        let wires = out["wires"].as_array().unwrap();
        assert_eq!(wires.len(), 2, "{out}");
        assert_eq!(wires[0]["arrowId"], e1.as_str());
        assert_eq!(wires[0]["predicate"], "supports");
        assert_eq!(wires[0]["source"], json!({"documentId": source}));
        assert_eq!(wires[0]["target"], json!({"iri": iri}));
        assert_eq!(wires[1]["source"], json!({"documentId": doc, "blockId": p}));
        assert_eq!(wires[1]["target"], json!({"documentId": doc, "blockId": q}));
        let wire1 = wires[0]["wireId"].as_str().unwrap().to_string();
        let wire2 = wires[1]["wireId"].as_str().unwrap().to_string();
        assert!(wire1.starts_with("wire-") && wire1 != wire2);

        // The arrows are marked, green, and say the predicate when they had no label.
        let raw = h.elements(&doc);
        let mark = &element(&raw, &e1)["customData"]["mnemosyneWire"];
        assert_eq!(mark["wireId"], wire1.as_str());
        assert_eq!(mark["predicate"], "supports");
        assert_eq!(mark["strokeWas"], "#1d1c19");
        assert_eq!(mark["labelFromPredicate"], true);
        assert_eq!(element(&raw, &e1)["strokeColor"], "#326f4c");
        assert_eq!(labelled_text(&raw, &e1).as_deref(), Some("supports"));
        let mark = &element(&raw, &e2)["customData"]["mnemosyneWire"];
        assert_eq!(mark["labelFromPredicate"], false);
        assert_eq!(
            labelled_text(&raw, &e2).as_deref(),
            Some("relates"),
            "its own label stays"
        );
        let read = h.read(&doc);
        let wired = read["arrows"]
            .as_array()
            .unwrap()
            .iter()
            .find(|x| x["id"] == e1.as_str())
            .unwrap();
        assert_eq!(
            wired["wire"],
            json!({"wireId": wire1, "predicate": "supports"})
        );

        // The stored wires: a document end and an IRI end; two block ends of the board.
        let on_source = h.call("get_wires", json!({"documentId": source})).unwrap();
        let stored = on_source["wires"]
            .as_array()
            .unwrap()
            .iter()
            .find(|w| w["id"] == wire1.as_str())
            .unwrap_or_else(|| panic!("wire {wire1} on the source document: {on_source}"));
        assert_eq!(stored["sourceDocumentId"], source.as_str());
        assert_eq!(stored["targetIri"], iri.as_str());
        assert_eq!(stored["predicate"], "supports");
        assert_eq!(stored["sceneDocumentId"], doc.as_str());
        assert_eq!(stored["sceneElementId"], e1.as_str());
        let on_board = h.call("get_wires", json!({"documentId": doc})).unwrap();
        let stored = on_board["wires"]
            .as_array()
            .unwrap()
            .iter()
            .find(|w| w["id"] == wire2.as_str())
            .unwrap_or_else(|| panic!("wire {wire2} on the board: {on_board}"));
        assert_eq!(stored["sourceDocumentId"], doc.as_str());
        assert_eq!(stored["sourceBlockId"], p.as_str());
        assert_eq!(stored["targetBlockId"], q.as_str());
        assert_eq!(stored["sceneElementId"], e2.as_str());
        let _ = (a, c);
    });
}

#[test]
fn wire_move_refuses_a_dangling_arrow() {
    with_profile("wbs-g5-dangling", || {
        let h = Harness::new("wbs-g5-dangling");
        let doc = h.create("Dangling");
        let out = h
            .apply(
                &doc,
                json!([
                    {"op":"add","ref":"a","shape":"box","text":"A"},
                    {"op":"add","ref":"b","shape":"box","text":"B","near":{"ref":"a","side":"right"}},
                    {"op":"add","ref":"c","shape":"box","text":"C","near":{"ref":"a","side":"below"}},
                    {"op":"add","ref":"d","shape":"box","text":"D","near":{"ref":"c","side":"right"}},
                    {"op":"add","ref":"e","shape":"box","text":"E","near":{"ref":"c","side":"below"}},
                    {"op":"add","ref":"f","shape":"box","text":"F","near":{"ref":"e","side":"right"}},
                    {"op":"connect","ref":"unbound","from":"a","to":"b"},
                    {"op":"connect","ref":"deleted","from":"c","to":"d"},
                    {"op":"connect","ref":"wired","from":"e","to":"f"}
                ]),
            )
            .unwrap();
        let id = |name: &str| out["created"][name].as_str().unwrap().to_string();
        let (a, d) = (id("a"), id("d"));
        let (unbound, deleted, wired) = (id("unbound"), id("deleted"), id("wired"));
        // A person unbinds one end, deletes a shape the way Excalidraw does (the arrow stays), and an arrow
        // that is already a wire.
        patch_element(&h, &doc, &unbound, |el| {
            el.insert("endBinding".into(), Value::Null);
        });
        patch_element(&h, &doc, &d, |el| {
            el.insert("isDeleted".into(), json!(true));
        });
        patch_element(&h, &doc, &wired, |el| {
            el.insert(
                "customData".into(),
                json!({"mnemosyneWire": {"wireId": "wire-x", "predicate": "supports",
                                         "strokeWas": "#1d1c19", "labelFromPredicate": false}}),
            );
        });
        let before = h.elements(&doc);
        let dangling = "has a dangling end: bind both ends before making a wire";
        for (moves, needle) in [
            (
                json!([{"op":"wire","arrow":unbound,"predicate":"supports"}]),
                dangling.to_string(),
            ),
            (
                json!([{"op":"wire","arrow":deleted,"predicate":"supports"}]),
                dangling.to_string(),
            ),
            (
                json!([{"op":"wire","arrow":wired,"predicate":"supports"}]),
                format!("arrow '{wired}' is already a wire: unwire it first"),
            ),
            (
                json!([{"op":"wire","arrow":a,"predicate":"supports"}]),
                format!("'{a}' is not an arrow"),
            ),
            (
                json!([{"op":"wire","arrow":deleted,"predicate":" "}]),
                "wire needs a predicate".to_string(),
            ),
        ] {
            let error = h.apply(&doc, moves).expect_err(&needle);
            assert!(error.contains(&needle), "{needle}: {error}");
        }
        // A later valid move in the same call does not rescue an earlier refusal: all or nothing.
        let error = h
            .apply(
                &doc,
                json!([
                    {"op":"update","id":a,"text":"Changed"},
                    {"op":"wire","arrow":unbound,"predicate":"supports"}
                ]),
            )
            .expect_err("refused");
        assert!(error.contains(dangling), "{error}");
        let after = h.elements(&doc);
        assert_eq!(after["elements"], before["elements"], "nothing was written");
        assert_eq!(after["revision"], before["revision"]);
        let stored = h.call("get_wires", json!({"documentId": doc})).unwrap();
        assert_eq!(stored["count"], 0, "no wire was stored: {stored}");
    });
}

// needs G3: its validation refuses a draft whose IRI is not absolute, which is what makes the store step fail.
#[test]
fn wire_move_failure_unmarks_the_arrow() {
    with_profile("wbs-g5-unmark", || {
        let h = Harness::new("wbs-g5-unmark");
        let doc = h.create("Unmark");
        let out = h
            .apply(
                &doc,
                json!([
                    {"op":"add","ref":"a","shape":"box","text":"Origin"},
                    {"op":"add","ref":"b","shape":"box","text":"Target","near":{"ref":"a","side":"right"}},
                    {"op":"connect","ref":"e","from":"a","to":"b"}
                ]),
            )
            .unwrap();
        let (a, e) = (
            out["created"]["a"].as_str().unwrap().to_string(),
            out["created"]["e"].as_str().unwrap().to_string(),
        );
        // A shape that points at an object whose IRI is not absolute (a board a person edited by hand).
        patch_element(&h, &doc, &a, |el| {
            el.insert(
                "link".into(),
                json!(super::links::object_link("wbs-g5-unmark", "not absolute")),
            );
        });
        let before = element(&h.elements(&doc), &e).clone();
        let error = h
            .apply(
                &doc,
                json!([{"op":"wire","arrow":e,"predicate":"supports"}]),
            )
            .expect_err("the store refuses the draft");
        assert!(error.contains("wire:"), "{error}");
        let raw = h.elements(&doc);
        let arrow = element(&raw, &e);
        assert!(
            arrow["customData"].get("mnemosyneWire").is_none(),
            "the mark is gone: {arrow}"
        );
        assert_eq!(
            arrow["strokeColor"], before["strokeColor"],
            "the stroke it had"
        );
        assert_eq!(
            labelled_text(&raw, &e),
            None,
            "the label the move gave is removed again"
        );
        let stored = h.call("get_wires", json!({"documentId": doc})).unwrap();
        assert_eq!(stored["count"], 0, "{stored}");
        // The board can be wired later, once the shape is fixed: nothing is stuck half-way.
        patch_element(&h, &doc, &a, |el| {
            el.insert("link".into(), Value::Null);
        });
        h.apply(
            &doc,
            json!([{"op":"wire","arrow":e,"predicate":"supports"}]),
        )
        .expect("wires once the end is sound");
    });
}

// ============================================================ ELK: the layout move through the tool

#[test]
fn layout_through_the_tool_draws_a_diagram_in_one_call_and_reports_it() {
    with_profile("wb-layout", || {
        let h = Harness::new("wb-layout");
        let doc = h.create("Dream-work");
        let out = h
            .apply(
                &doc,
                json!([
                    {"op":"add","ref":"w","shape":"box","text":"the dream-work"},
                    {"op":"add","ref":"c","shape":"note","text":"condensation"},
                    {"op":"add","ref":"d","shape":"note","text":"displacement"},
                    {"op":"add","ref":"m","shape":"ellipse","text":"manifest content"},
                    {"op":"connect","from":"w","to":"c"},
                    {"op":"connect","from":"w","to":"d","label":"displaces"},
                    {"op":"connect","from":"c","to":"m"},
                    {"op":"connect","from":"d","to":"m"},
                    {"op":"layout"}
                ]),
            )
            .expect("draw and lay out in one call");
        let report = &out["layouts"][0];
        assert_eq!(report["move"], 9);
        assert_eq!(report["nodes"], 4);
        assert_eq!(report["edges"], 4);
        assert_eq!(report["style"], "flow");
        assert_eq!(report["direction"], "right");
        assert!(report["ms"].is_u64(), "{report}");

        let raw = h.elements(&doc);
        let elements = raw["elements"].as_array().unwrap();
        let arrows: Vec<&Value> = elements.iter().filter(|e| e["type"] == "arrow").collect();
        assert_eq!(arrows.len(), 4);
        for arrow in &arrows {
            let (x, y) = (arrow["x"].as_f64().unwrap(), arrow["y"].as_f64().unwrap());
            let pts: Vec<(f64, f64)> = arrow["points"]
                .as_array()
                .unwrap()
                .iter()
                .map(|p| (x + p[0].as_f64().unwrap(), y + p[1].as_f64().unwrap()))
                .collect();
            for w in pts.windows(2) {
                assert!(
                    (w[1].0 - w[0].0).abs() <= 0.01 || (w[1].1 - w[0].1).abs() <= 0.01,
                    "a laid-out arrow runs at right angles: {arrow}"
                );
            }
            assert_eq!(arrow["startBinding"]["mode"], "orbit");
            assert_eq!(arrow["endBinding"]["mode"], "orbit");
        }
        // The outline still reads the four shapes, at their new places.
        let read = h.read(&doc);
        assert_eq!(read["shapes"].as_array().unwrap().len(), 4);

        let too_many = h
            .apply(&doc, json!([{"op":"layout"},{"op":"layout"},{"op":"layout"},{"op":"layout"},{"op":"layout"}]))
            .unwrap_err();
        assert!(too_many.contains("at most 4 layout moves"), "{too_many}");

        let revision = h.read(&doc)["revision"].clone();
        let w = out["created"]["w"].as_str().unwrap();
        let refused = h
            .apply(&doc, json!([{"op":"move","id":w,"by":{"dx":40,"dy":0}},{"op":"layout","style":"spiral"}]))
            .unwrap_err();
        assert!(refused.contains("move 2: style is one of"), "{refused}");
        assert_eq!(
            h.read(&doc)["revision"],
            revision,
            "a refused call writes nothing"
        );
    });
}

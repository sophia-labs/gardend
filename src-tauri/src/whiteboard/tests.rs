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
        assert_eq!(record["blocks"], json!([]));
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

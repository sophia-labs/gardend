//! The block contract through the real store: every JSON write path runs the
//! one normaliser (document-primitives §2, first slice step 4).
//!
//! The web editor has no container lists and no node for unknown blocks; on
//! open, the collaborative binding deleted whatever its schema rejected
//! (the 2026-09-25 editor list-loss investigation: 24 shapes, senderos lists lost).
//! gardend normalised only `content` strings; `tiptapJson` on
//! `write_document`, `insert_blocks`, `create_document_once` and the `blocks`
//! array stored the caller's JSON verbatim. These tests drive the real MCP
//! handlers against a disposable graph and read the stored document back: a
//! nested bullet/ordered/task list lands as flat `listItem{listType, indent}`,
//! an image inside a paragraph or a list item is hoisted, an unknown block is
//! wrapped as a byte-exact `opaqueBlock`, an unknown mark is kept verbatim —
//! and every rewrite is reported in the write's `warnings` with its node path.
use serde_json::{json, Value};

const GRAPH: &str = "block-contract";

fn with_graph(test: impl FnOnce(crate::app_runtime::AppHandle)) {
    let _serial = crate::tauri_runtime::profile_env_serial()
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let previous = std::env::var_os("GARDEN_PROFILE_DIR");
    let profile =
        std::env::temp_dir().join(format!("garden-block-contract-{}", uuid::Uuid::new_v4()));
    std::env::set_var("GARDEN_PROFILE_DIR", &profile);
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let app = crate::tauri_runtime::build_mock_app_for_tests(true);
        crate::graph_service::create_graph_service(
            &app,
            crate::graph_service::CreateGraphInput {
                graph_id: Some(GRAPH.into()),
                title: "Block contract specimen".into(),
                description: None,
                operation_id: None,
            },
        )
        .unwrap();
        test(app);
    }));
    match previous {
        Some(value) => std::env::set_var("GARDEN_PROFILE_DIR", value),
        None => std::env::remove_var("GARDEN_PROFILE_DIR"),
    }
    let _ = std::fs::remove_dir_all(&profile);
    if let Err(panic) = outcome {
        std::panic::resume_unwind(panic);
    }
}

fn text(t: &str) -> Value {
    json!({"type": "text", "text": t})
}

fn para(t: &str) -> Value {
    json!({"type": "paragraph", "content": [text(t)]})
}

fn item(children: Vec<Value>) -> Value {
    json!({"type": "listItem", "content": children})
}

/// The unknown block, kept as a value so the test can compare the stored
/// `originalJson` against its exact serialization.
fn callout() -> Value {
    json!({"type": "callout", "attrs": {"data-block-id": "co", "tone": "warn"},
           "content": [para("CALLOUT-TEXT")]})
}

/// One specimen covering every failure class of the list-loss battery that a
/// JSON write can carry: container lists (nested), invalid children, an
/// unknown block, unknown marks.
fn specimen() -> Vec<Value> {
    vec![
        json!({"type": "paragraph", "attrs": {"data-block-id": "anchor"},
               "content": [text("Anchor paragraph stays.")]}),
        json!({"type": "bulletList", "attrs": {"data-block-id": "bl"}, "content": [
            item(vec![para("BULLET-A"), json!({"type": "bulletList", "content": [
                item(vec![para("BULLET-A1")]),
            ]})]),
            item(vec![para("BULLET-B")]),
        ]}),
        json!({"type": "orderedList", "attrs": {"data-block-id": "ol"}, "content": [
            item(vec![para("ORDERED-A")]),
            item(vec![para("ORDERED-B")]),
        ]}),
        json!({"type": "taskList", "attrs": {"data-block-id": "tl"}, "content": [
            {"type": "taskItem", "attrs": {"checked": true}, "content": [para("TASK-A")]},
            {"type": "taskItem", "attrs": {"checked": false}, "content": [para("TASK-B")]},
        ]}),
        json!({"type": "paragraph", "attrs": {"data-block-id": "pimg"}, "content": [
            text("IMG-BEFORE "),
            {"type": "image", "attrs": {"src": "https://example.org/a.png", "alt": "ALT-A"}},
            text(" IMG-AFTER"),
        ]}),
        json!({"type": "listItem", "attrs": {"data-block-id": "li-img", "listType": "bullet", "indent": 0},
               "content": [para("LI-TEXT"),
                   {"type": "image", "attrs": {"src": "https://example.org/b.png", "alt": "ALT-B"}}]}),
        callout(),
        json!({"type": "paragraph", "attrs": {"data-block-id": "pm"}, "content": [
            {"type": "text", "text": "KBD-TEXT", "marks": [{"type": "kbd"}]},
            text(" and "),
            {"type": "text", "text": "SUP-TEXT", "marks": [{"type": "superscript"}, {"type": "bold"}]},
        ]}),
    ]
}

fn walk<'a>(node: &'a Value, out: &mut Vec<&'a Value>) {
    out.push(node);
    for child in node["content"].as_array().into_iter().flatten() {
        walk(child, out);
    }
}

fn nodes(doc: &Value) -> Vec<&Value> {
    let mut out = Vec::new();
    walk(doc, &mut out);
    out
}

fn node_text(node: &Value) -> String {
    if node["type"] == "text" {
        return node["text"].as_str().unwrap_or("").to_string();
    }
    node["content"]
        .as_array()
        .into_iter()
        .flatten()
        .map(node_text)
        .collect()
}

fn stored(app: &crate::app_runtime::AppHandle, document_id: &str) -> Value {
    let read = crate::document_mcp_service::mcp_local_read_document(
        app.clone(),
        &json!({"graph_id": GRAPH, "document_id": document_id, "format": "json", "maxChars": 1_000_000}),
    )
    .unwrap();
    let parsed: Value = serde_json::from_str(read["content"].as_str().unwrap()).unwrap();
    parsed["tiptapJson"].clone()
}

fn markdown(app: &crate::app_runtime::AppHandle, document_id: &str) -> String {
    crate::document_mcp_service::mcp_local_read_document(
        app.clone(),
        &json!({"graph_id": GRAPH, "document_id": document_id, "format": "markdown", "maxChars": 1_000_000}),
    )
    .unwrap()["content"]
        .as_str()
        .unwrap()
        .to_string()
}

fn warnings(result: &Value) -> Vec<String> {
    result["warnings"]
        .as_array()
        .unwrap_or_else(|| panic!("write result carries no warnings array: {result}"))
        .iter()
        .map(|w| w.as_str().unwrap_or_default().to_string())
        .collect()
}

async fn create(app: &crate::app_runtime::AppHandle, document_id: &str) {
    crate::mcp_workspace_crdt_service::mcp_local_create_document(
        app.clone(),
        &json!({"graphId": GRAPH, "documentId": document_id, "title": document_id}),
    )
    .await
    .unwrap();
}

async fn write_tiptap(app: &crate::app_runtime::AppHandle, document_id: &str, content: Vec<Value>) -> Value {
    crate::document_mutation_service::mcp_local_write_document(
        app.clone(),
        &json!({"graph_id": GRAPH, "document_id": document_id,
                "tiptapJson": {"type": "doc", "content": content}}),
    )
    .await
    .unwrap()
}

fn incarnation(app: &crate::app_runtime::AppHandle) -> String {
    crate::graph_record_store::read_graph_record_no_heal(app, GRAPH)
        .unwrap()
        .1
        .incarnation_id
        .unwrap()
}

/// Object keys sorted and integral floats as integers: the store's Y.Doc
/// round trip reorders attrs and reads numbers back as f64.
fn semantic(value: &Value) -> Value {
    match value {
        Value::Object(map) => {
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort();
            Value::Object(keys.into_iter().map(|k| (k.clone(), semantic(&map[k]))).collect())
        }
        Value::Array(items) => Value::Array(items.iter().map(semantic).collect()),
        Value::Number(n) => match n.as_f64() {
            Some(f) if f.fract() == 0.0 => json!(f as i64),
            _ => value.clone(),
        },
        other => other.clone(),
    }
}

/// What every path must have stored for `specimen()`. `carrier` is the
/// receipt path prefix of the caller's carrier (`content` or `blocks`).
fn assert_specimen_canonical(doc: &Value, result: &Value, path: &str) {
    assert_specimen_canonical_in(doc, result, path, "content")
}

fn assert_specimen_canonical_in(doc: &Value, result: &Value, path: &str, carrier: &str) {
    let all = nodes(doc);
    for forbidden in ["bulletList", "orderedList", "taskList", "taskItem", "callout"] {
        assert!(
            all.iter().all(|n| n["type"] != forbidden),
            "{path}: stored document still holds a {forbidden}: {doc}"
        );
    }
    // Lists: flat top-level listItems with listType/indent, text intact.
    let top = doc["content"].as_array().unwrap();
    let list_item = |label: &str| -> &Value {
        top.iter()
            .find(|n| n["type"] == "listItem" && node_text(n) == label)
            .unwrap_or_else(|| panic!("{path}: no top-level listItem {label:?} in {doc}"))
    };
    for (label, list_type, indent) in [
        ("BULLET-A", "bullet", 0),
        ("BULLET-A1", "bullet", 1),
        ("BULLET-B", "bullet", 0),
        ("ORDERED-A", "ordered", 0),
        ("ORDERED-B", "ordered", 0),
        ("TASK-A", "task", 0),
        ("TASK-B", "task", 0),
    ] {
        let li = list_item(label);
        assert_eq!(li["attrs"]["listType"], list_type, "{path}: {label}: {li}");
        assert_eq!(li["attrs"]["indent"].as_f64(), Some(indent as f64), "{path}: {label}: {li}");
        assert!(
            li["attrs"]["data-block-id"].as_str().is_some_and(|id| !id.is_empty()),
            "{path}: flattened {label} has a block id: {li}"
        );
    }
    assert_eq!(list_item("TASK-A")["attrs"]["checked"], true, "{path}");
    assert_eq!(list_item("TASK-B")["attrs"]["checked"], false, "{path}");
    // The container's id stays addressable: it moves to the first item.
    assert_eq!(list_item("BULLET-A")["attrs"]["data-block-id"], "bl", "{path}");
    assert_eq!(list_item("ORDERED-A")["attrs"]["data-block-id"], "ol", "{path}");
    assert_eq!(list_item("TASK-A")["attrs"]["data-block-id"], "tl", "{path}");

    // Block atoms never sit inside a paragraph or a listItem.
    for parent in &all {
        if parent["type"] == "paragraph" || parent["type"] == "listItem" {
            for child in parent["content"].as_array().into_iter().flatten() {
                assert_ne!(child["type"], "image", "{path}: image left inside {parent}");
            }
        }
    }
    let top_images = top.iter().filter(|n| n["type"] == "image").count();
    assert_eq!(top_images, 2, "{path}: both images hoisted to the top level: {doc}");
    let flat = serde_json::to_string(doc).unwrap();
    for needle in ["IMG-BEFORE", "IMG-AFTER", "LI-TEXT", "Anchor paragraph stays."] {
        assert!(flat.contains(needle), "{path}: {needle} lost: {doc}");
    }
    // The hoisted image follows the text before it and precedes the text after.
    let pos = |pred: &dyn Fn(&Value) -> bool| top.iter().position(|n| pred(n)).unwrap();
    let before = pos(&|n: &Value| node_text(n).contains("IMG-BEFORE"));
    let image = pos(&|n: &Value| n["type"] == "image" && n["attrs"]["alt"] == "ALT-A");
    let after = pos(&|n: &Value| node_text(n).contains("IMG-AFTER"));
    assert!(before < image && image < after, "{path}: hoist keeps order: {doc}");
    assert_eq!(top[before]["attrs"]["data-block-id"], "pimg", "{path}: first part keeps the id");

    // Unknown block → opaqueBlock, byte-exact original, same block id.
    let opaque = top
        .iter()
        .find(|n| n["type"] == "opaqueBlock")
        .unwrap_or_else(|| panic!("{path}: no opaqueBlock in {doc}"));
    assert_eq!(opaque["attrs"]["originalType"], "callout", "{path}");
    assert_eq!(
        opaque["attrs"]["originalJson"].as_str().unwrap(),
        serde_json::to_string(&callout()).unwrap(),
        "{path}: originalJson is the verbatim node"
    );
    assert!(opaque["attrs"]["text"].as_str().unwrap().contains("CALLOUT-TEXT"), "{path}");
    assert_eq!(opaque["attrs"]["contractVersion"].as_f64(), Some(1.0), "{path}");
    assert_eq!(opaque["attrs"]["data-block-id"], "co", "{path}");
    assert!(opaque.get("content").is_none(), "{path}: opaque is a leaf");

    // Unknown marks preserved verbatim, known ones untouched.
    let kbd = all.iter().find(|n| n["text"] == "KBD-TEXT").unwrap();
    assert_eq!(kbd["marks"], json!([{"type": "kbd"}]), "{path}");
    let sup = all.iter().find(|n| n["text"] == "SUP-TEXT").unwrap();
    // Written as [superscript, bold]. Y keeps a run's marks as an unordered
    // attribute map, so the store holds the set and every read returns the
    // contract's canonical order (`canonical_mark_cmp`): the contract's marks
    // in contract order, then unknown marks by name. Not a coin flip per read.
    assert_eq!(sup["marks"], json!([{"type": "bold"}, {"type": "superscript"}]), "{path}");

    // Every rewrite is reported, with its node path.
    let warnings = warnings(result);
    let joined = warnings.join("\n");
    for needle in [
        "bulletList", "orderedList", "taskList", "callout", "opaqueBlock", "image", "kbd",
        "superscript", &format!("{carrier}[1]"), &format!("{carrier}[6]"),
    ] {
        assert!(joined.contains(needle), "{path}: warnings name {needle:?}:\n{joined}");
    }
}

fn is_rewrite(warning: &str) -> bool {
    warning.starts_with("block-contract known-equivalent") || warning.starts_with("block-contract unknown")
}

#[test]
fn write_document_tiptap_json_is_normalised_with_receipts() {
    with_graph(|app| {
        crate::app_runtime::async_runtime::block_on(async {
            create(&app, "doc-write").await;
            let result = write_tiptap(&app, "doc-write", specimen()).await;
            let doc = stored(&app, "doc-write");
            assert_specimen_canonical(&doc, &result, "write_document.tiptapJson");

            // Idempotent: writing the stored canonical content back rewrites
            // nothing. (The normaliser's byte-for-byte law is proven on JSON in
            // block_contract_tests; a store read reorders Y attrs, so here
            // equality is semantic.)
            let again = write_tiptap(
                &app,
                "doc-write",
                doc["content"].as_array().unwrap().clone(),
            )
            .await;
            let rewrites: Vec<String> = warnings(&again).into_iter().filter(|w| is_rewrite(w)).collect();
            assert!(rewrites.is_empty(), "canonical content was rewritten: {rewrites:?}");
            assert_eq!(
                semantic(&stored(&app, "doc-write")),
                semantic(&doc),
                "re-normalising canonical content changed it"
            );

            // The markdown face never drops the opaque object.
            let md = markdown(&app, "doc-write");
            assert!(md.contains("```json"), "opaque renders as a fenced JSON block:\n{md}");
            assert!(md.contains("CALLOUT-TEXT"), "{md}");
            for label in ["BULLET-A", "BULLET-A1", "ORDERED-B", "TASK-A", "KBD-TEXT", "SUP-TEXT"] {
                assert!(md.contains(label), "markdown lost {label}:\n{md}");
            }
        });
    });
}

#[test]
fn write_document_blocks_array_of_tiptap_nodes_is_normalised() {
    with_graph(|app| {
        crate::app_runtime::async_runtime::block_on(async {
            create(&app, "doc-blocks").await;
            // MCP write_document takes content or tiptapJson; the blocks
            // array reaches document.write from the hosted/CRDT surface.
            let result = crate::crdt_queue::enqueue_crdt_operation(
                app.clone(),
                crate::crdt_operation_types::EnqueueCrdtOperationInput {
                    kind: "document.write".into(),
                    graph_id: GRAPH.into(),
                    document_id: Some("doc-blocks".into()),
                    payload: json!({"documentId": "doc-blocks", "blocks": specimen()}),
                },
            )
            .await
            .unwrap();
            // The TipTap-shaped entries (content is an array) go through the
            // contract instead of collapsing to empty paragraphs.
            let doc = stored(&app, "doc-blocks");
            assert_specimen_canonical_in(&doc, &result, "document.write.blocks", "blocks");
        });
    });
}

#[test]
fn insert_blocks_tiptap_json_is_normalised_with_receipts() {
    with_graph(|app| {
        crate::app_runtime::async_runtime::block_on(async {
            create(&app, "doc-insert").await;
            let result = crate::mcp_block_mutation_service::mcp_local_insert_blocks(
                app.clone(),
                &json!({"graph_id": GRAPH, "document_id": "doc-insert",
                        "tiptapJson": {"type": "doc", "content": specimen()}}),
            )
            .await
            .unwrap();
            let doc = stored(&app, "doc-insert");
            assert_specimen_canonical(&doc, &result, "insert_blocks.tiptapJson");
        });
    });
}

#[test]
fn insert_blocks_blocks_array_of_tiptap_nodes_is_normalised() {
    with_graph(|app| {
        crate::app_runtime::async_runtime::block_on(async {
            create(&app, "doc-insert-blocks").await;
            let result = crate::mcp_block_mutation_service::mcp_local_insert_blocks(
                app.clone(),
                &json!({"graph_id": GRAPH, "document_id": "doc-insert-blocks", "blocks": specimen()}),
            )
            .await
            .unwrap();
            let doc = stored(&app, "doc-insert-blocks");
            assert_specimen_canonical_in(&doc, &result, "insert_blocks.blocks", "blocks");
        });
    });
}

#[test]
fn create_document_once_is_normalised_with_receipts() {
    with_graph(|app| {
        crate::app_runtime::async_runtime::block_on(async {
            let inc = incarnation(&app);
            let result = crate::document_create_once_mcp::create_document_once(
                app.clone(),
                &json!({"graph_id": GRAPH, "graphIncarnation": inc, "document_id": "doc-once",
                        "title": "Once", "order": 1, "parentId": null, "awaitDurable": false,
                        "tiptapJson": {"type": "doc", "content": specimen()}}),
            )
            .await
            .unwrap();
            let doc = stored(&app, "doc-once");
            assert_specimen_canonical(&doc, &result, "create_document_once");
        });
    });
}

#[test]
fn canonical_content_is_stored_verbatim_without_receipts() {
    with_graph(|app| {
        crate::app_runtime::async_runtime::block_on(async {
            create(&app, "doc-canonical").await;
            // What the web editor itself writes: flat list items, a table, a
            // block image, math, a query block, a known-marked run.
            let canonical = vec![
                json!({"type": "heading", "attrs": {"data-block-id": "h", "level": 2}, "content": [text("Title")]}),
                json!({"type": "listItem", "attrs": {"data-block-id": "l1", "listType": "bullet", "indent": 0},
                       "content": [para("one")]}),
                json!({"type": "listItem", "attrs": {"data-block-id": "l2", "listType": "task", "indent": 1, "checked": true},
                       "content": [para("two")]}),
                json!({"type": "table", "content": [{"type": "tableRow", "content": [
                    {"type": "tableHeader", "content": [para("H")]},
                    {"type": "tableCell", "content": [para("C")]}]}]}),
                json!({"type": "image", "attrs": {"data-block-id": "img", "src": "https://example.org/c.png"}}),
                json!({"type": "mathBlock", "attrs": {"data-block-id": "m", "src": "x^2"}}),
                json!({"type": "queryBlock", "attrs": {"data-block-id": "q", "query": "SELECT * WHERE {}"}}),
                json!({"type": "paragraph", "attrs": {"data-block-id": "p"}, "content": [
                    {"type": "text", "text": "bold", "marks": [{"type": "bold"}]},
                    {"type": "hardBreak"},
                    {"type": "wikilink", "attrs": {"label": "L", "targetDocId": "d"}},
                    {"type": "mathInline", "attrs": {"src": "y"}}]}),
            ];
            let result = write_tiptap(&app, "doc-canonical", canonical.clone()).await;
            assert!(warnings(&result).is_empty(), "canonical content drew receipts: {result}");
            assert_eq!(
                semantic(&stored(&app, "doc-canonical")["content"]),
                semantic(&Value::Array(canonical)),
            );
        });
    });
}

#[test]
fn opaque_block_projects_as_rdf_with_its_original_type() {
    with_graph(|app| {
        crate::app_runtime::async_runtime::block_on(async {
            create(&app, "doc-rdf").await;
            write_tiptap(&app, "doc-rdf", vec![para("lead"), callout()]).await;
            let rows = crate::rdf_service::mcp_local_sparql_query(
                app.clone(),
                &json!({"graphId": GRAPH, "query":
                    "PREFIX mdoc: <http://mnemosyne.dev/doc#> SELECT ?b ?t WHERE { GRAPH ?g { ?b a mdoc:OpaqueBlock ; mdoc:originalType ?t } }"}),
            )
            .await
            .unwrap();
            let flat = rows.to_string();
            assert!(flat.contains("callout") && flat.contains("block-co"), "OpaqueBlock not projected: {flat}");
        });
    });
}

/// The 24-shape list-loss battery, every tiptapJson through `write_document`
/// and `create_document_once` (and `insert_blocks` where its top-level id
/// rule admits it): no text lost, nothing the editor schema rejects left in
/// the stored document, and re-normalising is a no-op.
#[test]
fn list_loss_battery_through_every_tiptap_json_path() {
    let battery: Vec<Value> = serde_json::from_str(include_str!(
        "../tests/fixtures/block_contract/list-loss-battery.json"
    ))
    .unwrap();
    with_graph(|app| {
        crate::app_runtime::async_runtime::block_on(async {
            let mut report = Vec::new();
            let mut failures = Vec::new();
            for (index, shape) in battery.iter().enumerate() {
                let Some(content) = shape["body"]["tiptapJson"]["content"].as_array() else {
                    continue;
                };
                let id = shape["id"].as_str().unwrap();
                let texts: Vec<String> = content
                    .iter()
                    .flat_map(|n| {
                        let mut v = Vec::new();
                        walk(n, &mut v);
                        v.into_iter()
                            .filter_map(|n| n["text"].as_str().map(str::to_string))
                            .collect::<Vec<_>>()
                    })
                    .filter(|t| !t.trim().is_empty())
                    .collect();
                for path in ["write_document", "create_document_once", "insert_blocks"] {
                    let doc_id = format!("{id}-{}", &path[..6]);
                    let outcome: Result<Value, String> = match path {
                        "write_document" => {
                            create(&app, &doc_id).await;
                            Ok(write_tiptap(&app, &doc_id, content.clone()).await)
                        }
                        "create_document_once" => {
                            crate::document_create_once_mcp::create_document_once(
                                app.clone(),
                                &json!({"graph_id": GRAPH, "graphIncarnation": incarnation(&app),
                                        "document_id": doc_id, "title": "Battery", "order": index,
                                        "parentId": null, "awaitDurable": false,
                                        "tiptapJson": {"type": "doc", "content": content}}),
                            )
                            .await
                        }
                        _ => {
                            create(&app, &doc_id).await;
                            crate::mcp_block_mutation_service::mcp_local_insert_blocks(
                                app.clone(),
                                &json!({"graph_id": GRAPH, "document_id": doc_id,
                                        "tiptapJson": {"type": "doc", "content": content}}),
                            )
                            .await
                        }
                    };
                    let result = match outcome {
                        Ok(result) => result,
                        Err(error) => {
                            report.push(format!("{id:<24} {path:<22} refused: {}", error.chars().take(90).collect::<String>()));
                            continue;
                        }
                    };
                    let doc = stored(&app, &doc_id);
                    let flat = serde_json::to_string(&doc).unwrap();
                    let lost: Vec<&String> = texts.iter().filter(|t| !flat.contains(t.as_str())).collect();
                    let bad: Vec<String> = nodes(&doc)
                        .iter()
                        .filter_map(|n| n["type"].as_str())
                        .filter(|t| !crate::crdt_engine::block_contract::is_known_node(t))
                        .map(str::to_string)
                        .collect();
                    let (renormalised, again) =
                        crate::crdt_engine::block_contract::normalise_doc_content(
                            doc["content"].as_array().unwrap(),
                            "idempotence",
                        );
                    let idempotent = again.iter().all(|w| !is_rewrite(w))
                        && serde_json::to_string(&renormalised).unwrap()
                            == serde_json::to_string(&doc["content"]).unwrap();
                    let receipts = warnings(&result).len();
                    report.push(format!(
                        "{id:<24} {path:<22} lost={} unknown={bad:?} idempotent={idempotent} receipts={receipts}",
                        lost.len()
                    ));
                    if !lost.is_empty() || !bad.is_empty() || !idempotent {
                        failures.push(format!("{id} via {path}: lost={lost:?} unknown={bad:?} idempotent={idempotent} doc={doc}"));
                    }
                }
            }
            println!("BLOCK_CONTRACT_BATTERY\n{}", report.join("\n"));
            assert!(failures.is_empty(), "battery failures:\n{}", failures.join("\n"));
        });
    });
}

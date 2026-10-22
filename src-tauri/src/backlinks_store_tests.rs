//! G4's store tests for `get_backlinks` (INTERFACES.md): a real graph, real wires, real documents, the real store.
//! Owned by G4.
//!
//! Wires, embeds and wikilinks are written through the real tools and projected by the real materializers. The
//! whiteboard lane (shapes, cards, arrows) is written here as the INTERFACES.md section 5 triples, straight into the
//! store's named graph for the lane: the lane writer is another unit's, and what `get_backlinks` reads is the shape
//! of those triples. `dispatch` goes through the registry entry a live `tools/call` takes.
use std::sync::Arc;

use oxigraph::model::{GraphName, Literal, NamedNode, Quad, Term};
use serde_json::{json, Value};

use crate::local_jobs::LocalJobRegistry;
use crate::mcp_dispatch_registry::{lookup, McpCallCtx};

const GRAPH: &str = "wb-graph";
const OBJECT: &str = "urn:mnemosyne:local:graph:wb-graph:dream:wolf-man";
const WB: &str = "http://mnemosyne.dev/whiteboard#";
const RDF_TYPE: &str = "http://www.w3.org/1999/02/22-rdf-syntax-ns#type";

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

    /// `get_backlinks` rows, through the dispatch table.
    fn backlinks(&self, args: Value) -> Vec<Value> {
        self.call("get_backlinks", args).unwrap()["backlinks"]
            .as_array()
            .cloned()
            .unwrap_or_default()
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
    let profile = std::env::temp_dir().join(format!("garden-backlinks-{}", uuid::Uuid::new_v4()));
    std::env::set_var("GARDEN_PROFILE_DIR", &profile);
    let jobs_dir =
        std::env::temp_dir().join(format!("garden-backlinks-jobs-{}", uuid::Uuid::new_v4()));
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let app = crate::tauri_runtime::build_mock_app_for_tests(true);
        crate::graph_service::create_graph_service(
            &app,
            crate::graph_service::CreateGraphInput {
                graph_id: Some(GRAPH.into()),
                title: "Backlinks".into(),
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

fn write_tiptap(env: &Env, document_id: &str, content: Vec<Value>) {
    run(crate::document_mutation_service::mcp_local_write_document(
        env.app.clone(),
        &json!({"graph_id": GRAPH, "document_id": document_id,
                "tiptapJson": {"type": "doc", "content": content}}),
    ))
    .unwrap();
}

fn create_wire(env: &Env, wire: Value) {
    env.call("create_wires", json!({"wires": [wire]})).unwrap();
}

fn doc_subject(document_id: &str) -> String {
    format!("urn:mnemosyne:local:document:{document_id}")
}

/// A whiteboard document (the real tool), returning its id.
fn create_board(env: &Env, title: &str) -> String {
    env.call("whiteboard_create", json!({"title": title}))
        .unwrap()["documentId"]
        .as_str()
        .unwrap()
        .to_string()
}

// ---------------------------------------------------------------------------
// The whiteboard lane, as INTERFACES.md section 5 writes it
// ---------------------------------------------------------------------------

struct Lane<'a> {
    env: &'a Env,
    graph: String,
}

impl<'a> Lane<'a> {
    fn new(env: &'a Env) -> Self {
        Self::in_graph(
            env,
            &format!("urn:mnemosyne:local:graph:{GRAPH}:projection:whiteboard"),
        )
    }

    /// A second lane graph, to show a duplicate quad in another named graph reports once.
    fn in_graph(env: &'a Env, graph: &str) -> Self {
        Self {
            env,
            graph: graph.to_string(),
        }
    }

    fn quad(&self, subject: &str, predicate: &str, object: Term) -> Quad {
        Quad::new(
            NamedNode::new(subject).unwrap(),
            NamedNode::new(predicate).unwrap(),
            object,
            GraphName::NamedNode(NamedNode::new(&self.graph).unwrap()),
        )
    }

    fn put(&self, subject: &str, predicate: &str, object: Term) {
        let dir = crate::paths::existing_graph_dir(&self.env.app, GRAPH).unwrap();
        let store = crate::rdf_store_service::open_graph_store(&dir).unwrap();
        store
            .insert(&self.quad(subject, predicate, object))
            .unwrap();
    }

    fn node(iri: &str) -> Term {
        Term::NamedNode(NamedNode::new(iri).unwrap())
    }

    fn text(value: &str) -> Term {
        Term::Literal(Literal::new_simple_literal(value))
    }

    /// `B(x)` = `D#block-x`.
    fn block(board: &str, element: &str) -> String {
        format!("{}#block-{element}", doc_subject(board))
    }

    /// `A(x)` = `D#arrow-x`.
    fn arrow(board: &str, element: &str) -> String {
        format!("{}#arrow-{element}", doc_subject(board))
    }

    /// A shape or card on the board, optionally referring to something.
    fn element(&self, board: &str, element: &str, class: &str, refers_to: Option<&str>) {
        let subject = Self::block(board, element);
        self.put(&subject, RDF_TYPE, Self::node(&format!("{WB}{class}")));
        self.put(&subject, &format!("{WB}documentId"), Self::text(board));
        self.put(
            &subject,
            &format!("{WB}onBoard"),
            Self::node(&doc_subject(board)),
        );
        if let Some(target) = refers_to {
            self.put(&subject, &format!("{WB}refersTo"), Self::node(target));
        }
    }

    fn connection(&self, board: &str, element: &str, from: &str, to: &str) {
        let subject = Self::arrow(board, element);
        self.put(&subject, RDF_TYPE, Self::node(&format!("{WB}Connection")));
        self.put(&subject, &format!("{WB}documentId"), Self::text(board));
        self.put(
            &subject,
            &format!("{WB}onBoard"),
            Self::node(&doc_subject(board)),
        );
        self.put(
            &subject,
            &format!("{WB}from"),
            Self::node(&Self::block(board, from)),
        );
        self.put(
            &subject,
            &format!("{WB}to"),
            Self::node(&Self::block(board, to)),
        );
    }
}

fn field<'a>(row: &'a Value, key: &str) -> &'a str {
    row[key]
        .as_str()
        .unwrap_or_else(|| panic!("{key} missing in {row}"))
}

// ---------------------------------------------------------------------------
// G4
// ---------------------------------------------------------------------------

/// An incoming wire is a backlink; an outgoing one, a self-wire and a bidirectional wire's `-inv` twin are not.
/// A block-narrowed lookup finds the wires that end at that block. An object-source scene wire is sourced at its
/// board and arrow.
#[test]
fn backlinks_find_an_incoming_wire() {
    with_env(|env| {
        create(env, "doc-a");
        create(env, "doc-b");
        create_wire(
            env,
            json!({"wireId": "wire-ab", "sourceDocumentId": "doc-a", "sourceBlockId": "p1",
                   "targetDocumentId": "doc-b", "targetBlockId": "p9", "predicate": "supports"}),
        );
        // An outgoing wire of doc-b, and a wire from doc-b to itself: neither is a backlink of doc-b.
        create_wire(
            env,
            json!({"wireId": "wire-ba", "sourceDocumentId": "doc-b", "targetDocumentId": "doc-a",
                   "predicate": "supports"}),
        );
        create_wire(
            env,
            json!({"wireId": "wire-bb", "sourceDocumentId": "doc-b", "targetDocumentId": "doc-b",
                   "predicate": "supports"}),
        );

        let rows = env.backlinks(json!({"documentId": "doc-b"}));
        assert_eq!(rows.len(), 1, "one incoming wire: {rows:?}");
        let row = &rows[0];
        assert_eq!(row["kind"], "wire");
        assert_eq!(field(row, "sourceDocumentId"), "doc-a");
        assert_eq!(field(row, "sourceBlockId"), "p1");
        assert_eq!(field(row, "wireId"), "wire-ab");
        assert_eq!(field(row, "predicate"), "supports");
        assert_eq!(field(row, "sourceTitle"), "doc-a");

        // The response says what was asked, and counts.
        let answer = env
            .call("get_backlinks", json!({"documentId": "doc-b"}))
            .unwrap();
        assert_eq!(answer["count"], 1);
        assert_eq!(answer["graphId"], GRAPH);
        assert_eq!(answer["target"]["documentId"], "doc-b");

        // A bidirectional wire reports once: its `-inv` twin is not a second backlink.
        create_wire(
            env,
            json!({"wireId": "wire-both", "sourceDocumentId": "doc-a", "targetDocumentId": "doc-b",
                   "predicate": "related", "bidirectional": true}),
        );
        let rows = env.backlinks(json!({"documentId": "doc-b"}));
        let both: Vec<&Value> = rows
            .iter()
            .filter(|r| {
                r["wireId"]
                    .as_str()
                    .is_some_and(|id| id.starts_with("wire-both"))
            })
            .collect();
        assert_eq!(both.len(), 1, "the -inv twin is skipped: {rows:?}");
        assert_eq!(field(both[0], "wireId"), "wire-both");

        // Narrowed to a block: only the wire that ends at it.
        let narrowed = env.backlinks(json!({"documentId": "doc-b", "blockId": "p9"}));
        assert_eq!(narrowed.len(), 1, "{narrowed:?}");
        assert_eq!(field(&narrowed[0], "wireId"), "wire-ab");
        assert!(env
            .backlinks(json!({"documentId": "doc-b", "blockId": "nope"}))
            .is_empty());

        // Nothing points at doc-c.
        create(env, "doc-c");
        assert!(env.backlinks(json!({"documentId": "doc-c"})).is_empty());
    });
}

/// A document block that embeds an object by IRI is a backlink of the object, from the embedding block.
#[test]
fn backlinks_find_an_embed() {
    with_env(|env| {
        create(env, "doc-notes");
        write_tiptap(
            env,
            "doc-notes",
            vec![
                json!({"type": "paragraph", "attrs": {"data-block-id": "p1"},
                       "content": [{"type": "text", "text": "A dream."}]}),
                json!({"type": "objectEmbed", "attrs": {
                    "data-block-id": "emb1", "iri": OBJECT, "slot": "card",
                    "label": "The Wolf Man dream", "classLabel": "Dream report"}}),
            ],
        );

        let rows = env.backlinks(json!({"iri": OBJECT}));
        assert_eq!(rows.len(), 1, "{rows:?}");
        assert_eq!(rows[0]["kind"], "embed");
        assert_eq!(field(&rows[0], "sourceDocumentId"), "doc-notes");
        assert_eq!(field(&rows[0], "sourceBlockId"), "emb1");
        assert_eq!(field(&rows[0], "sourceTitle"), "doc-notes");

        // The embedding document is not itself back-linked to by the embed.
        assert!(env.backlinks(json!({"documentId": "doc-notes"})).is_empty());
    });
}

/// A `[[wikilink]]` is a backlink of the document it names; its block is the paragraph that holds it. A wikilink
/// that names another graph's document of the same id is not.
#[test]
fn backlinks_find_a_wikilink() {
    with_env(|env| {
        create(env, "doc-source");
        create(env, "doc-target");
        write_tiptap(
            env,
            "doc-source",
            vec![
                json!({"type": "paragraph", "attrs": {"data-block-id": "p-link"}, "content": [
                    {"type": "text", "text": "See "},
                    {"type": "wikilink", "attrs": {"targetDocId": "doc-target", "targetGraphId": GRAPH, "label": "the target"}},
                ]}),
            ],
        );
        create(env, "doc-elsewhere");
        write_tiptap(
            env,
            "doc-elsewhere",
            vec![
                json!({"type": "paragraph", "attrs": {"data-block-id": "p-far"}, "content": [
                    {"type": "wikilink", "attrs": {"targetDocId": "doc-target", "targetGraphId": "another-graph", "label": "far"}},
                ]}),
            ],
        );

        let rows = env.backlinks(json!({"documentId": "doc-target"}));
        assert_eq!(rows.len(), 1, "only the same-graph wikilink: {rows:?}");
        assert_eq!(rows[0]["kind"], "wikilink");
        assert_eq!(field(&rows[0], "sourceDocumentId"), "doc-source");
        assert_eq!(field(&rows[0], "sourceBlockId"), "p-link");
    });
}

/// A board shape that refers to a document is a `board-link`; a card that refers to an object is a `board-card`;
/// an arrow that ends at such a shape gives a `board-connection` naming the arrow. A shape that refers to its own
/// board says nothing.
#[test]
fn backlinks_find_a_board_link_and_card() {
    with_env(|env| {
        create(env, "doc-traumdeutung");
        let board = create_board(env, "Dream-work board");
        let lane = Lane::new(env);
        lane.element(
            &board,
            "lnk0000000000000000001",
            "Shape",
            Some(&doc_subject("doc-traumdeutung")),
        );
        lane.element(&board, "crd0000000000000000001", "Card", Some(OBJECT));
        lane.element(&board, "box0000000000000000001", "Shape", None);
        lane.element(
            &board,
            "self000000000000000001",
            "Shape",
            Some(&doc_subject(&board)),
        );
        lane.connection(
            &board,
            "arw0000000000000000002",
            "lnk0000000000000000001",
            "crd0000000000000000001",
        );

        // The document: a shape refers to it, and the arrow leaves that shape.
        let rows = env.backlinks(json!({"documentId": "doc-traumdeutung"}));
        let kinds: Vec<&str> = rows.iter().map(|r| field(r, "kind")).collect();
        assert_eq!(kinds, vec!["board-link", "board-connection"], "{rows:?}");
        for row in &rows {
            assert_eq!(field(row, "sourceDocumentId"), board);
            assert_eq!(field(row, "sourceBlockId"), "lnk0000000000000000001");
            assert_eq!(field(row, "sourceKind"), "whiteboard", "{row}");
            assert_eq!(field(row, "sourceTitle"), "Dream-work board");
        }
        assert!(rows[0].get("viaConnection").is_none());
        assert_eq!(field(&rows[1], "viaConnection"), "arw0000000000000000002");

        // The object: the card refers to it, and the arrow ends at that card.
        let rows = env.backlinks(json!({"iri": OBJECT}));
        let kinds: Vec<&str> = rows.iter().map(|r| field(r, "kind")).collect();
        assert_eq!(kinds, vec!["board-card", "board-connection"], "{rows:?}");
        assert_eq!(field(&rows[0], "sourceBlockId"), "crd0000000000000000001");
        assert_eq!(field(&rows[1], "viaConnection"), "arw0000000000000000002");

        // The board itself: its own shape that refers to it is dropped.
        assert!(env.backlinks(json!({"documentId": board})).is_empty());
    });
}

/// An object IRI collects every kind at once: a wire (from a scene wire drawn on a board), an embed and a board
/// card, in the contract's kind order.
#[test]
fn backlinks_for_an_object_iri() {
    with_env(|env| {
        create(env, "doc-traumdeutung");
        create(env, "doc-notes");
        let board = create_board(env, "Dream-work board");
        write_tiptap(
            env,
            "doc-notes",
            vec![json!({"type": "objectEmbed", "attrs": {
                "data-block-id": "emb1", "iri": OBJECT, "slot": "card",
                "label": "The Wolf Man dream", "classLabel": "Dream report"}})],
        );
        create_wire(
            env,
            json!({"wireId": "wire-wb-0001", "sourceDocumentId": "doc-traumdeutung", "targetIri": OBJECT,
                   "predicate": "supports", "sceneGraphId": GRAPH, "sceneDocumentId": board,
                   "sceneElementId": "arw0000000000000000002",
                   "sceneSourceElementId": "lnk0000000000000000001",
                   "sceneTargetElementId": "crd0000000000000000001"}),
        );
        Lane::new(env).element(&board, "crd0000000000000000001", "Card", Some(OBJECT));

        let rows = env.backlinks(json!({"iri": OBJECT}));
        let kinds: Vec<&str> = rows.iter().map(|r| field(r, "kind")).collect();
        assert_eq!(kinds, vec!["wire", "embed", "board-card"], "{rows:?}");
        assert_eq!(field(&rows[0], "sourceDocumentId"), "doc-traumdeutung");
        assert_eq!(field(&rows[0], "wireId"), "wire-wb-0001");
        assert_eq!(field(&rows[0], "predicate"), "supports");
        assert_eq!(field(&rows[1], "sourceDocumentId"), "doc-notes");
        assert_eq!(field(&rows[2], "sourceDocumentId"), board);

        // An object nobody points at, and an IRI that is not one, answer empty lists.
        assert!(env
            .backlinks(json!({"iri": "urn:mnemosyne:local:graph:wb-graph:dream:nobody"}))
            .is_empty());
        assert!(env.backlinks(json!({"iri": "not an iri"})).is_empty());
    });
}

/// An object source (a scene wire from a card) is sourced at its board and the arrow's source element.
#[test]
fn backlinks_for_an_object_source_scene_wire() {
    with_env(|env| {
        create(env, "doc-target");
        let board = create_board(env, "Board");
        create_wire(
            env,
            json!({"wireId": "wire-obj", "sourceIri": OBJECT, "targetDocumentId": "doc-target",
                   "predicate": "supports", "sceneGraphId": GRAPH, "sceneDocumentId": board,
                   "sceneElementId": "arw1", "sceneSourceElementId": "crd1", "sceneTargetElementId": "lnk1"}),
        );
        let rows = env.backlinks(json!({"documentId": "doc-target"}));
        assert_eq!(rows.len(), 1, "{rows:?}");
        assert_eq!(field(&rows[0], "sourceDocumentId"), board);
        assert_eq!(field(&rows[0], "sourceBlockId"), "crd1");
        assert_eq!(field(&rows[0], "wireId"), "wire-obj");
    });
}

/// Rows are ordered (kind, then source document) and a duplicate fact, here the same shape asserted in a second
/// named graph, reports once. Two calls answer the same.
#[test]
fn backlinks_are_sorted_and_deduplicated() {
    with_env(|env| {
        create(env, "doc-target");
        create(env, "doc-b");
        create(env, "doc-a");
        // Created out of order.
        create_wire(
            env,
            json!({"wireId": "w-b", "sourceDocumentId": "doc-b", "targetDocumentId": "doc-target", "predicate": "supports"}),
        );
        create_wire(
            env,
            json!({"wireId": "w-a", "sourceDocumentId": "doc-a", "targetDocumentId": "doc-target", "predicate": "supports"}),
        );
        let board = create_board(env, "Board");
        let lane = Lane::new(env);
        lane.element(&board, "lnk1", "Shape", Some(&doc_subject("doc-target")));
        // The same shape, asserted again in another named graph of the same store.
        let again = Lane::in_graph(
            env,
            &format!("urn:mnemosyne:local:graph:{GRAPH}:projection:whiteboard-copy"),
        );
        again.element(&board, "lnk1", "Shape", Some(&doc_subject("doc-target")));

        let first = env.backlinks(json!({"documentId": "doc-target"}));
        let summary: Vec<(String, String)> = first
            .iter()
            .map(|r| {
                (
                    field(r, "kind").to_string(),
                    field(r, "sourceDocumentId").to_string(),
                )
            })
            .collect();
        assert_eq!(
            summary,
            vec![
                ("wire".to_string(), "doc-a".to_string()),
                ("wire".to_string(), "doc-b".to_string()),
                ("board-link".to_string(), board.clone()),
            ],
            "{first:?}"
        );
        let second = env.backlinks(json!({"documentId": "doc-target"}));
        assert_eq!(first, second, "the same question answers the same");
    });
}

/// Exactly one of `documentId` and `iri`: neither is an error, both is an error, and `blockId` goes with a
/// document. The service and the dispatch table agree.
#[test]
fn backlinks_refuse_neither_or_both_targets() {
    with_env(|env| {
        create(env, "doc-a");
        let neither = env.call("get_backlinks", json!({})).unwrap_err();
        assert!(
            neither.contains("exactly one of documentId and iri"),
            "{neither}"
        );
        let both = env
            .call(
                "get_backlinks",
                json!({"documentId": "doc-a", "iri": OBJECT}),
            )
            .unwrap_err();
        assert!(both.contains("exactly one of documentId and iri"), "{both}");
        let block_with_iri = env
            .call("get_backlinks", json!({"iri": OBJECT, "blockId": "p1"}))
            .unwrap_err();
        assert!(
            block_with_iri.contains("blockId goes with documentId"),
            "{block_with_iri}"
        );

        // The service itself, without the dispatch.
        let direct = crate::backlinks_service::mcp_local_get_backlinks(
            env.app.clone(),
            &json!({"graphId": GRAPH}),
        )
        .unwrap_err();
        assert!(
            direct.contains("exactly one of documentId and iri"),
            "{direct}"
        );

        // A well-formed question about an unknown graph is an empty answer, not an error.
        let unknown = env
            .call(
                "get_backlinks",
                json!({"graphId": "no-such-graph", "documentId": "doc-a"}),
            )
            .unwrap();
        assert_eq!(unknown["count"], 0);
    });
}

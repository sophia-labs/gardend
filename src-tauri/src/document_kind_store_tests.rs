//! A document's kind reaches the graph. Found 2026-10-18: `create_document`
//! over MCP passed only id, title, parent, order and ifExists to
//! `workspace.createDocument`, so a `documentKind` (Shrubbery sends
//! "daily-note"; a workflow sends "workflow") was dropped before the writer,
//! and the workspace projection emitted a kind only for flow boards. These
//! tests drive the real MCP handlers against a disposable graph and read the
//! answer back with SPARQL, the way Shrubbery's daily notes ask for it.
use serde_json::{json, Value};

const GRAPH: &str = "document-kind";

fn with_graph(test: impl FnOnce(crate::app_runtime::AppHandle)) {
    let _serial = crate::tauri_runtime::profile_env_serial()
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let previous = std::env::var_os("GARDEN_PROFILE_DIR");
    let profile =
        std::env::temp_dir().join(format!("garden-document-kind-{}", uuid::Uuid::new_v4()));
    std::env::set_var("GARDEN_PROFILE_DIR", &profile);
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let app = crate::tauri_runtime::build_mock_app_for_tests(true);
        crate::graph_service::create_graph_service(
            &app,
            crate::graph_service::CreateGraphInput {
                graph_id: Some(GRAPH.into()),
                title: "Document kind specimen".into(),
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

async fn create(app: &crate::app_runtime::AppHandle, arguments: Value) {
    let mut arguments = arguments;
    arguments["graphId"] = json!(GRAPH);
    crate::mcp_workspace_crdt_service::mcp_local_create_document(app.clone(), &arguments)
        .await
        .unwrap_or_else(|error| panic!("create_document {arguments}: {error}"));
}

/// `(document id, kind)` for every document the projection gives a
/// `doc:documentKind`.
async fn kinds(app: &crate::app_runtime::AppHandle) -> Vec<(String, String)> {
    let result = crate::rdf_service::mcp_local_sparql_query(
        app.clone(),
        &json!({"graphId": GRAPH, "query":
            "PREFIX doc: <http://mnemosyne.dev/doc#> \
             SELECT ?doc ?kind WHERE { GRAPH ?g { ?doc a doc:TipTapDocument ; doc:documentKind ?kind } } \
             ORDER BY ?doc"}),
    )
    .await
    .unwrap();
    let flat = result.to_string();
    let rows = result["rows"]
        .as_array()
        .cloned()
        .unwrap_or_else(|| panic!("no rows in SPARQL answer: {flat}"));
    rows.iter()
        .map(|row| {
            // Cells are terms: `<iri>` or `"literal"` (maybe `^^<datatype>`).
            let term = |name: &str| {
                let raw = row[name]
                    .as_str()
                    .unwrap_or_else(|| panic!("row without {name}: {flat}"));
                match raw.strip_prefix('"') {
                    Some(rest) => rest.rsplit_once('"').map_or(rest, |(value, _)| value),
                    None => raw.trim_start_matches('<').trim_end_matches('>'),
                }
                .to_string()
            };
            let doc = term("doc");
            let id = doc
                .strip_prefix("urn:mnemosyne:local:document:")
                .unwrap_or(&doc)
                .to_string();
            (id, term("kind"))
        })
        .collect()
}

#[test]
fn create_document_kind_projects_as_doc_document_kind() {
    with_graph(|app| {
        crate::app_runtime::async_runtime::block_on(async {
            create(
                &app,
                json!({"documentId": "doc-wf", "title": "Plan", "documentKind": "workflow"}),
            )
            .await;
            create(
                &app,
                json!({
                    "documentId": "daily-note-2026-10-18", "title": "Sunday",
                    "documentKind": "daily-note",
                }),
            )
            .await;
            create(&app, json!({"documentId": "doc-plain", "title": "Plain"})).await;
            create(
                &app,
                json!({"documentId": "doc-blank", "title": "Blank", "documentKind": ""}),
            )
            .await;

            assert_eq!(
                kinds(&app).await,
                vec![
                    (
                        "daily-note-2026-10-18".to_string(),
                        "daily-note".to_string()
                    ),
                    ("doc-wf".to_string(), "workflow".to_string()),
                ]
            );
        });
    });
}

/// Absent = untouched: a later `create_document` with `ifExists: "update"`
/// that names no kind keeps the kind the document already has.
#[test]
fn update_without_kind_keeps_the_kind() {
    with_graph(|app| {
        crate::app_runtime::async_runtime::block_on(async {
            create(
                &app,
                json!({"documentId": "doc-wf", "title": "Plan", "documentKind": "workflow"}),
            )
            .await;
            create(
                &app,
                json!({"documentId": "doc-wf", "title": "Plan, renamed", "ifExists": "update"}),
            )
            .await;
            assert_eq!(
                kinds(&app).await,
                vec![("doc-wf".to_string(), "workflow".to_string())]
            );
        });
    });
}

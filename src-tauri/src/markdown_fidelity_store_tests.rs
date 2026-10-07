//! Markdown fidelity through the real store: `create_document` +
//! `write_document(format=markdown)` into a disposable graph, then
//! `read_document` as markdown and as TipTap JSON — the exact MCP handlers the
//! hosted cell serves (hosted Hoja QA 2026-09-23 found tables and images lost
//! at import, H4 → H3, and a markdown read-back stripped of every mark, link,
//! nesting level, ordered number and multi-line quote).
//!
//! The pure parse/serialize legs and the corpus live in
//! tests/markdown_round_trip.rs; this proves the stored document (Y.Doc →
//! canonical TipTap JSON) keeps what the parser produced and that the
//! markdown face is rendered from it.
use serde_json::{json, Value};

const GRAPH: &str = "markdown-fidelity";

fn corpus(name: &str) -> String {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/markdown_round_trip")
        .join(name);
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
}

fn normalize(markdown: &str) -> String {
    markdown
        .lines()
        .map(str::trim_end)
        .collect::<Vec<_>>()
        .join("\n")
        .trim()
        .to_string()
}

fn count(node: &Value, node_type: &str) -> usize {
    let own = usize::from(node["type"] == node_type);
    own + node["content"]
        .as_array()
        .map(|c| c.iter().map(|n| count(n, node_type)).sum())
        .unwrap_or(0)
}

#[test]
fn markdown_write_read_round_trip_through_the_store() {
    let _serial = crate::tauri_runtime::profile_env_serial()
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let previous = std::env::var_os("GARDEN_PROFILE_DIR");
    let profile = std::env::temp_dir().join(format!("garden-md-fidelity-{}", uuid::Uuid::new_v4()));
    std::env::set_var("GARDEN_PROFILE_DIR", &profile);
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let app = crate::tauri_runtime::build_mock_app_for_tests(true);
        crate::graph_service::create_graph_service(
            &app,
            crate::graph_service::CreateGraphInput {
                graph_id: Some(GRAPH.into()),
                title: "Markdown fidelity specimen".into(),
                description: None,
                operation_id: None,
            },
        )
        .unwrap();
        crate::app_runtime::async_runtime::block_on(async {
            let cases = [
                ("qa-table", "qa-table.md", None),
                ("qa-image", "qa-image.md", None),
                ("qa-lists", "qa-lists.md", None),
                ("qa-marks", "qa-marks.md", None),
                ("qa-links", "qa-links.md", None),
                ("qa-code", "qa-code.md", None),
                (
                    "qa-outline",
                    "qa-outline.md",
                    Some("qa-outline.expected.md"),
                ),
                ("constructs", "constructs.md", None),
            ];
            let mut failures = Vec::new();
            for (doc_id, source, expected) in cases {
                let authored = corpus(source);
                let expected = expected.map(corpus).unwrap_or_else(|| authored.clone());
                crate::mcp_workspace_crdt_service::mcp_local_create_document(
                    app.clone(),
                    &json!({"graphId": GRAPH, "documentId": doc_id, "title": doc_id}),
                )
                .await
                .unwrap();
                crate::document_mutation_service::mcp_local_write_document(
                    app.clone(),
                    &json!({
                        "graph_id": GRAPH, "document_id": doc_id,
                        "content": authored, "format": "markdown",
                    }),
                )
                .await
                .unwrap();
                let read = |format: &str| {
                    crate::document_mcp_service::mcp_local_read_document(
                        app.clone(),
                        &json!({
                            "graph_id": GRAPH, "document_id": doc_id,
                            "format": format, "maxChars": 1_000_000,
                        }),
                    )
                    .unwrap()["content"]
                        .as_str()
                        .unwrap()
                        .to_string()
                };
                let markdown = read("markdown");
                if normalize(&markdown) != normalize(&expected) {
                    failures.push(format!(
                        "--- {doc_id} expected ---\n{}\n--- {doc_id} read back ---\n{}",
                        normalize(&expected),
                        normalize(&markdown)
                    ));
                }
                let stored: Value = serde_json::from_str(&read("json")).unwrap();
                let tiptap = &stored["tiptapJson"];
                match doc_id {
                    "qa-table" => {
                        assert_eq!(count(tiptap, "table"), 1, "stored table: {tiptap}");
                        assert_eq!(count(tiptap, "tableHeader"), 3);
                        assert_eq!(count(tiptap, "tableCell"), 9);
                    }
                    "qa-image" => {
                        assert_eq!(count(tiptap, "image"), 1, "stored image: {tiptap}");
                    }
                    "qa-outline" => {
                        let levels: Vec<i64> = tiptap["content"]
                            .as_array()
                            .unwrap()
                            .iter()
                            .filter(|n| n["type"] == "heading")
                            .map(|n| n["attrs"]["level"].as_f64().unwrap() as i64)
                            .collect();
                        assert_eq!(levels, vec![1, 2, 3, 3, 2, 3], "schema clamps h4 to h3");
                    }
                    _ => {}
                }
            }
            assert!(
                failures.is_empty(),
                "store round trip lost content:\n{}",
                failures.join("\n\n")
            );
        });
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

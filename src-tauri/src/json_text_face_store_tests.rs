//! JSON authored as text through the real store: `create_document` +
//! `write_document(format: "plain")` into a disposable graph, then the two
//! read paths that serve the Markdown face — the MCP `read_document(format:
//! "markdown")` handler and the HTTP export route
//! (`GET /documents/{graph_id}/{document_id}/export?format=markdown`,
//! `hosted_document_export_response`). Both render through
//! `document_markdown` → `markdown_export::tiptap_json_to_markdown`, as do
//! the Emporium spine and applier (in-process render anchors). `get_block` /
//! `read_blocks` return the projected block text and never reach the
//! exporter.
//!
//! Since the block-contract engine (garden 3b1c9ea / c60f057, rolled
//! 2026-10-04 06:24Z) that face escaped `*`, backticks, non-intraword `_`,
//! `\"`... in JSON written as text: Phanes' byte-equal rereads of its
//! control-plane documents failed and Choreograph's `JSON.parse` of prompt
//! bindings and tool manifests threw. The pure legs, the rule and the cases
//! are in tests/json_text_face.rs.
use serde_json::{json, Value};

const GRAPH: &str = "json-text-face";

/// Phanes' control-bundle shape: canonical one-line JSON with `**bold**`,
/// inline backticks, non-intraword `_` and the JSON escapes `\n`, `\"`,
/// `\\` inside strings (the same specimen as tests/json_text_face.rs).
const CONTROL_BUNDLE: &str = r##"{"agent":{"kind":"agent","objectId":"urn:sophia:agent:agent-ded0c28b107012ad"},"object":{"kind":"agent-interaction-control-bundle","version":"sha256:c5a9b8c1"},"presentation":{"templates":[{"key":"privacy.saved","surface":"ephemeral","text":"Saved **{label}**. Use `/phanes privacy` to change it."},{"key":"attachment-suffix","surface":"attachment-suffix","text":"\n\n_…full response attached._"},{"key":"privacy.status","surface":"embed-body","text":"Current preference: **{status}**\n\n{transition}"},{"key":"quote","surface":"ephemeral","text":"She said \"hi\" \\ <b>bold</b> ~~old~~ [^1] a|b ]: x * y"}]},"schema":"sophia.agent-interaction-control-bundle.v1"}"##;

const PROSE: &str =
    "Plain prose keeps its escaping: *stars*, _under_ and `ticks`, but snake_case stays.";
const PROSE_FACE: &str =
    r"Plain prose keeps its escaping: \*stars\*, \_under\_ and \`ticks\`, but snake_case stays.";

fn prompt_binding() -> Value {
    json!({
        "agentId": "agent-ded0c28b107012ad",
        "bindingId": "apb_4ff7385bc7a532250fa9aed2",
        "inference": {"model": "google/gemini-2.5-flash", "provider": "openrouter"},
        "promptDocumentId": "phanes-prime-prompt-v1",
        "registeredName": "Phanes",
        "schema": "sophia.agent-prompt-binding.v1",
        "snapshot": {
            "blocks": [],
            "documentId": "phanes-prime-prompt-v1",
            "renderedDigest": "510adc10f0636022f5a8bbdeb693a7f77ff6c0b65e586f2c739686a800bff0e6",
            "renderedText": "You are Phanes.\n\nUse `discord__search_messages`, `discord__message_context`, and exact reads.\n\nReturn a `sophia.proposed-effect.v1`; quote the user \"exactly\" and _never_ guess a C:\\path or a **secret**.",
            "schema": "sophia.agent-prompt-snapshot.v1"
        },
        "status": "active"
    })
}

fn tool_manifest() -> Value {
    json!({
        "agentId": "agent-ded0c28b107012ad",
        "capabilities": [{
            "access": "write",
            "approvalPolicy": {"mode": "on-risk"},
            "backend": "garden-mcp",
            "capabilityId": "cap_mnemosyne",
            "tools": [{
                "description": "Search the *bounded* privacy projection; at most 50 rows, newest first.",
                "inputSchema": {
                    "properties": {
                        "handle": {"pattern": "^[a-z]+_[A-Za-z0-9_-]{24}$", "type": "string"},
                        "query": {"type": "string"}
                    },
                    "required": ["handle", "query"],
                    "type": "object"
                },
                "name": "discord__search_messages"
            }]
        }],
        "schema": "sophia.agent-tool-manifest.v1"
    })
}

fn with_graph(test: impl FnOnce(crate::app_runtime::AppHandle)) {
    let _serial = crate::tauri_runtime::profile_env_serial()
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let previous = std::env::var_os("GARDEN_PROFILE_DIR");
    let profile =
        std::env::temp_dir().join(format!("garden-json-text-face-{}", uuid::Uuid::new_v4()));
    std::env::set_var("GARDEN_PROFILE_DIR", &profile);
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let app = crate::tauri_runtime::build_mock_app_for_tests(true);
        crate::graph_service::create_graph_service(
            &app,
            crate::graph_service::CreateGraphInput {
                graph_id: Some(GRAPH.into()),
                title: "JSON text face specimen".into(),
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

async fn create_and_write(
    app: &crate::app_runtime::AppHandle,
    document_id: &str,
    content: &str,
    format: &str,
) {
    crate::mcp_workspace_crdt_service::mcp_local_create_document(
        app.clone(),
        &json!({"graphId": GRAPH, "documentId": document_id, "title": document_id}),
    )
    .await
    .unwrap();
    crate::document_mutation_service::mcp_local_write_document(
        app.clone(),
        &json!({
            "graph_id": GRAPH, "document_id": document_id,
            "content": content, "format": format,
        }),
    )
    .await
    .unwrap();
}

/// `read_document` through the MCP handler the hosted cell serves.
fn read(app: &crate::app_runtime::AppHandle, document_id: &str, format: &str) -> String {
    crate::document_mcp_service::mcp_local_read_document(
        app.clone(),
        &json!({
            "graph_id": GRAPH, "document_id": document_id,
            "format": format, "maxChars": 1_000_000,
        }),
    )
    .unwrap()["content"]
        .as_str()
        .unwrap()
        .to_string()
}

/// The stored paragraphs' text (from the canonical TipTap document), joined
/// the way the Markdown face joins blocks.
fn stored_text(app: &crate::app_runtime::AppHandle, document_id: &str) -> String {
    let stored: Value = serde_json::from_str(&read(app, document_id, "json")).unwrap();
    let blocks = stored["tiptapJson"]["content"]
        .as_array()
        .unwrap_or_else(|| panic!("{document_id}: no stored content: {stored}"));
    blocks
        .iter()
        .map(|block| {
            assert_eq!(block["type"], "paragraph", "{document_id}: stored block {block}");
            block["content"]
                .as_array()
                .into_iter()
                .flatten()
                .map(|node| node["text"].as_str().unwrap_or(""))
                .collect::<String>()
        })
        .filter(|text| !text.trim().is_empty())
        .collect::<Vec<_>>()
        .join("\n\n")
}

/// The HTTP export route's handler body.
async fn export_markdown(app: &crate::app_runtime::AppHandle, document_id: &str) -> String {
    let response = crate::document_hosted_projection::hosted_document_export_response(
        app,
        GRAPH,
        document_id,
        Some("markdown"),
        None,
    )
    .unwrap();
    let content_type = response
        .headers()
        .get(axum::http::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .map(str::to_string);
    assert_eq!(content_type.as_deref(), Some("text/markdown; charset=utf-8"));
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    String::from_utf8(bytes.to_vec()).unwrap()
}

#[test]
fn json_authored_as_plain_text_reads_back_verbatim_through_the_store() {
    with_graph(|app| {
        crate::app_runtime::async_runtime::block_on(async {
            // 1. Phanes' control bundle: write(plain) → read(markdown) is
            //    byte-equal, on the MCP face and on the HTTP export.
            create_and_write(&app, "phanes-control", CONTROL_BUNDLE, "plain").await;
            assert_eq!(
                stored_text(&app, "phanes-control"),
                CONTROL_BUNDLE,
                "the write stores the authored text"
            );
            assert_eq!(read(&app, "phanes-control", "markdown"), CONTROL_BUNDLE);
            assert_eq!(export_markdown(&app, "phanes-control").await, CONTROL_BUNDLE);

            // 2. Pretty-printed JSON (the Phanes seed's bytes): the plain
            //    write folds it into one paragraph; the face is that stored
            //    text, byte for byte, and JSON-equal to the write.
            for (document_id, value) in [
                ("phanes-prime-prompt-binding-v1", prompt_binding()),
                ("phanes-prime-tool-manifest-v3", tool_manifest()),
            ] {
                let source = format!("{}\n", serde_json::to_string_pretty(&value).unwrap());
                create_and_write(&app, document_id, &source, "plain").await;
                let stored = stored_text(&app, document_id);
                let markdown = read(&app, document_id, "markdown");
                assert_eq!(markdown, stored, "{document_id}: the face is the stored text");
                assert_eq!(
                    serde_json::from_str::<Value>(&markdown).unwrap(),
                    value,
                    "{document_id}: JSON-equal to the write"
                );
                assert_eq!(export_markdown(&app, document_id).await, stored, "{document_id}");
            }

            // 3. Prose keeps its escaping.
            create_and_write(&app, "prose", PROSE, "plain").await;
            assert_eq!(read(&app, "prose", "markdown"), PROSE_FACE);
            assert_eq!(export_markdown(&app, "prose").await, PROSE_FACE);

            // 4. The containment form (Phanes' control bundle since
            //    2026-10-05 23:00Z): a fenced json code block reads back as
            //    the same fence around the same bytes.
            let fenced = format!("```json\n{}\n```", CONTROL_BUNDLE);
            create_and_write(&app, "phanes-control-fenced", &format!("{}\n", fenced), "markdown")
                .await;
            assert_eq!(read(&app, "phanes-control-fenced", "markdown"), fenced);
        });
    });
}

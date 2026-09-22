use crate::{
    crdt_engine::document_ops::tiptap_json_from_blocks,
    document_block_rendering::{document_blocks_for_read, html_escape, render_block_content},
    document_service::DocumentRecord,
};

pub(super) fn document_json(document: &DocumentRecord) -> Result<String, String> {
    let tiptap_json = match document.tiptap_json.as_ref() {
        Some(tiptap_json) => tiptap_json.clone(),
        None => {
            let blocks = serde_json::to_value(document_blocks_for_read(document))
                .map_err(|error| format!("serialize document blocks for JSON export: {error}"))?;
            tiptap_json_from_blocks(Some(&blocks))
        }
    };
    serde_json::to_string_pretty(&tiptap_json)
        .map_err(|error| format!("serialize TipTap JSON export: {error}"))
}

pub(super) fn document_markdown(document: &DocumentRecord) -> String {
    let blocks = document_blocks_for_read(document);
    blocks
        .iter()
        .map(|block| match block.block_type.as_str() {
            "heading" => {
                let level = block.level.unwrap_or(1).clamp(1, 6) as usize;
                format!("{} {}", "#".repeat(level), block.content)
            }
            "codeBlock" | "code_block" => {
                let language = block.language.clone().unwrap_or_default();
                format!("```{language}\n{}\n```", block.content)
            }
            "code" => {
                let language = block.language.clone().unwrap_or_default();
                format!("```{language}\n{}\n```", block.content)
            }
            "blockquote" => format!("> {}", block.content),
            "quote" => format!("> {}", block.content),
            "todo" => {
                let marker = if block.checked.unwrap_or(false) {
                    "x"
                } else {
                    " "
                };
                format!("- [{marker}] {}", block.content)
            }
            "bullet" => format!("- {}", block.content),
            "numbered" => format!("1. {}", block.content),
            "divider" => "---".to_string(),
            _ => block.content.clone(),
        })
        .collect::<Vec<_>>()
        .join("\n\n")
}

pub(super) fn document_xml(document: &DocumentRecord) -> String {
    if !document.tiptap_xml.trim().is_empty() {
        return document.tiptap_xml.clone();
    }
    let body = document_blocks_for_read(document)
        .iter()
        .map(|block| render_block_content(block, "xml"))
        .collect::<Vec<_>>()
        .join("");
    format!("<doc>{body}</doc>")
}

fn document_export_theme_class(theme: Option<&str>) -> &'static str {
    match theme
        .unwrap_or("garden")
        .trim()
        .to_ascii_lowercase()
        .as_str()
    {
        "manuscript" => "theme-manuscript",
        "dusk" => "theme-dusk",
        "meridian" => "theme-meridian",
        "vesper" => "theme-vesper",
        _ => "theme-garden",
    }
}

pub(super) fn document_html(document: &DocumentRecord, theme: Option<&str>) -> String {
    let title = if document.title.trim().is_empty() {
        "Untitled"
    } else {
        document.title.as_str()
    };
    let body = document_blocks_for_read(document)
        .iter()
        .map(|block| match block.block_type.as_str() {
            "heading" => {
                let level = block.level.unwrap_or(1).clamp(1, 6);
                format!("<h{level}>{}</h{level}>", html_escape(&block.content))
            }
            "code" | "codeBlock" | "code_block" => {
                let language = block.language.clone().unwrap_or_default();
                let class_name = if language.trim().is_empty() {
                    String::new()
                } else {
                    format!(" class=\"language-{}\"", html_escape(language.trim()))
                };
                format!(
                    "<pre><code{class_name}>{}</code></pre>",
                    html_escape(&block.content)
                )
            }
            "blockquote" | "quote" => format!(
                "<blockquote><p>{}</p></blockquote>",
                html_escape(&block.content)
            ),
            "todo" => {
                let checked = if block.checked.unwrap_or(false) {
                    " checked"
                } else {
                    ""
                };
                format!(
                    "<p><input type=\"checkbox\" disabled{checked}> {}</p>",
                    html_escape(&block.content)
                )
            }
            "bullet" => format!("<ul><li>{}</li></ul>", html_escape(&block.content)),
            "numbered" => format!("<ol><li>{}</li></ol>", html_escape(&block.content)),
            "divider" => "<hr>".to_string(),
            _ => format!("<p>{}</p>", html_escape(&block.content)),
        })
        .collect::<Vec<_>>()
        .join("\n");
    let theme_class = document_export_theme_class(theme);
    format!(
        "<!doctype html><html><head><meta charset=\"utf-8\"><title>{}</title><style>body{{font-family:system-ui,-apple-system,BlinkMacSystemFont,\"Segoe UI\",sans-serif;line-height:1.55;max-width:760px;margin:48px auto;padding:0 24px;color:#1f2937;background:#ffffff}}pre{{padding:16px;overflow:auto;background:#111827;color:#f9fafb}}blockquote{{border-left:4px solid #94a3b8;margin-left:0;padding-left:16px;color:#475569}}.theme-dusk{{background:#111827;color:#e5e7eb}}.theme-dusk pre{{background:#020617}}.theme-vesper{{background:#1f1b24;color:#f5eef8}}.theme-manuscript{{font-family:Georgia,serif}}.theme-meridian h1,.theme-meridian h2,.theme-meridian h3{{color:#0f766e}}</style></head><body class=\"{theme_class}\"><h1>{}</h1>{body}</body></html>",
        html_escape(title),
        html_escape(title)
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        document_service::BlockSnapshot,
        rdf::document_subject,
        runtime_config::{DOCUMENT_SCHEMA_VERSION, LOCAL_GRAPH_ORIGIN, LOCAL_PROVIDER_ID},
    };

    fn test_block(id: &str, block_type: &str, content: &str) -> BlockSnapshot {
        BlockSnapshot {
            id: id.to_string(),
            block_type: block_type.to_string(),
            content: content.to_string(),
            parent_id: None,
            order: 0.0,
            level: if block_type == "heading" {
                Some(2)
            } else {
                None
            },
            checked: if block_type == "todo" {
                Some(true)
            } else {
                None
            },
            language: if block_type == "codeBlock" {
                Some("rust".to_string())
            } else {
                None
            },
            marks: Vec::new(),
        }
    }

    fn test_document(blocks: Vec<BlockSnapshot>) -> DocumentRecord {
        DocumentRecord {
            document_id: "doc-a".to_string(),
            graph_id: "graph-a".to_string(),
            title: "Document A".to_string(),
            revision: 1,
            body: "Fallback body".to_string(),
            origin: LOCAL_GRAPH_ORIGIN.to_string(),
            provider_id: LOCAL_PROVIDER_ID.to_string(),
            local_path: "/tmp/doc-a".to_string(),
            rdf_subject: document_subject("doc-a"),
            created_at: "1000".to_string(),
            updated_at: "2000".to_string(),
            capabilities: Vec::new(),
            schema_version: DOCUMENT_SCHEMA_VERSION,
            tiptap_xml: String::new(),
            tiptap_json: None,
            ydoc_update_base64: String::new(),
            ydoc_state_path: String::new(),
            tree: None,
            blocks,
            rdf_triple_count: 0,
            document_kind: None,
        }
    }

    #[test]
    fn document_rendering_preserves_common_block_shapes_and_escaping() {
        let document = test_document(vec![
            test_block("h", "heading", "Title"),
            test_block("code", "codeBlock", "let x = 1;"),
            test_block("todo", "todo", "done"),
            test_block("p", "paragraph", "<script>alert('x')</script>"),
        ]);

        assert_eq!(
            document_markdown(&document),
            "## Title\n\n```rust\nlet x = 1;\n```\n\n- [x] done\n\n<script>alert('x')</script>"
        );
        assert!(document_xml(&document).contains(
            r#"<paragraph data-block-id="p">&lt;script&gt;alert('x')&lt;/script&gt;</paragraph>"#
        ));
        let html = document_html(&document, Some("dusk"));
        assert!(html.contains(r#"class="theme-dusk""#));
        assert!(html.contains("&lt;script&gt;alert(&#39;x&#39;)&lt;/script&gt;"));
    }

    #[test]
    fn document_json_exports_the_canonical_tiptap_document() {
        let mut document = test_document(vec![test_block("p", "paragraph", "fallback")]);
        document.tiptap_json = Some(serde_json::json!({
            "type": "doc",
            "content": [{
                "type": "paragraph",
                "attrs": { "data-block-id": "canonical" },
                "content": [{ "type": "text", "text": "canonical body" }]
            }]
        }));

        let exported: serde_json::Value =
            serde_json::from_str(&document_json(&document).unwrap()).unwrap();
        assert_eq!(exported, document.tiptap_json.unwrap());
    }

    #[test]
    fn document_json_reconstructs_legacy_documents_from_block_projections() {
        let document = test_document(vec![test_block("p", "paragraph", "legacy body")]);

        let exported: serde_json::Value =
            serde_json::from_str(&document_json(&document).unwrap()).unwrap();
        assert_eq!(exported["type"], "doc");
        assert_eq!(exported["content"][0]["attrs"]["data-block-id"], "p");
        assert_eq!(exported["content"][0]["content"][0]["text"], "legacy body");
    }
}

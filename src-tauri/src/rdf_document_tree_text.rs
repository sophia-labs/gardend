use crate::document_service::TreeNodeSnapshot;

pub(super) fn normalize_node_text(node: &TreeNodeSnapshot) -> String {
    node_text(node)
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

fn node_text(node: &TreeNodeSnapshot) -> String {
    if node.kind == "text" {
        return node.text_content.clone().unwrap_or_default();
    }
    if node.tag_name.as_deref() == Some("hardBreak") {
        return "\n".to_string();
    }
    if let Some(text) = atom_node_text(node) {
        return text;
    }
    node.children
        .iter()
        .map(node_text)
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
}

fn atom_node_text(node: &TreeNodeSnapshot) -> Option<String> {
    match node.tag_name.as_deref() {
        Some("wikilink") => Some(
            node.attributes
                .extra
                .get("label")
                .or_else(|| node.attributes.extra.get("blockPreview"))
                .cloned()
                .unwrap_or_default(),
        ),
        Some("footnote") => node
            .attributes
            .footnote_content
            .as_ref()
            .map(|content| format!("[^{content}]")),
        Some("mathInline" | "mathBlock") => node
            .attributes
            .src
            .clone()
            .or_else(|| node.attributes.extra.get("src").cloned()),
        Some("image") => node
            .attributes
            .alt
            .clone()
            .or_else(|| node.attributes.extra.get("title").cloned())
            .or_else(|| node.attributes.src.clone()),
        Some("queryBlock") => {
            let parts = ["comment", "query"]
                .iter()
                .filter_map(|key| node.attributes.extra.get(*key))
                .filter(|value| !value.trim().is_empty())
                .cloned()
                .collect::<Vec<_>>();
            if parts.is_empty() {
                None
            } else {
                Some(parts.join("\n"))
            }
        }
        _ => None,
    }
}

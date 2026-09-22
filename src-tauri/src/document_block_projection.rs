use crate::{
    document_service::{BlockSnapshot, InlineMarkSnapshot},
    text_utils::text_preview,
};

fn hosted_block_type(block_type: &str) -> &'static str {
    match block_type {
        "heading" => "heading",
        "bullet" | "bulletList" | "listItem" => "bullet",
        "orderedList" | "numbered" => "numbered",
        "taskItem" | "todo" => "todo",
        "blockquote" | "quote" => "quote",
        "codeBlock" | "code_block" | "code" => "code",
        "horizontalRule" | "divider" => "divider",
        _ => "paragraph",
    }
}

fn hosted_mark_value(mark: &InlineMarkSnapshot) -> serde_json::Value {
    let mut value = serde_json::json!({
        "id": mark.id,
        "type": mark.mark_type,
        "start": mark.start,
        "end": mark.end,
    });
    if let Some(href) = &mark.href {
        value["href"] = serde_json::json!(href);
    }
    if let Some(target_doc_id) = &mark.target_doc_id {
        value["target_doc_id"] = serde_json::json!(target_doc_id);
    }
    if let Some(label) = &mark.label {
        value["label"] = serde_json::json!(label);
    }
    value
}

pub(super) fn hosted_block_value(block: &BlockSnapshot) -> serde_json::Value {
    serde_json::json!({
        "id": block.id,
        "type": hosted_block_type(&block.block_type),
        "content": block.content,
        "parentId": block.parent_id,
        "order": block.order,
        "level": block.level,
        "checked": block.checked,
        "language": block.language,
        "marks": block.marks.iter().map(hosted_mark_value).collect::<Vec<_>>(),
    })
}

pub(super) fn hosted_block_summary_value(block: &BlockSnapshot) -> serde_json::Value {
    serde_json::json!({
        "id": block.id,
        "type": hosted_block_type(&block.block_type),
        "level": block.level,
        "text": block.content,
        "preview": text_preview(&block.content, 64),
    })
}

pub(super) fn hosted_block_context_item(
    block: &BlockSnapshot,
    is_target: bool,
) -> serde_json::Value {
    serde_json::json!({
        "id": block.id,
        "type": hosted_block_type(&block.block_type),
        "level": block.level,
        "text": block.content,
        "preview": text_preview(&block.content, 96),
        "is_target": is_target,
    })
}

fn block_context(blocks: &[BlockSnapshot], index: usize) -> serde_json::Value {
    serde_json::json!({
        "previousBlockId": index.checked_sub(1).and_then(|previous| blocks.get(previous)).map(|block| block.id.clone()),
        "nextBlockId": blocks.get(index + 1).map(|block| block.id.clone()),
    })
}

pub(super) fn block_json(
    block: &BlockSnapshot,
    index: usize,
    blocks: &[BlockSnapshot],
) -> serde_json::Value {
    serde_json::json!({
        "block_id": block.id.clone(),
        "blockId": block.id.clone(),
        "index": index,
        "type": block.block_type.clone(),
        "block_type": block.block_type.clone(),
        "blockType": block.block_type.clone(),
        "content": block.content.clone(),
        "text_content": block.content.clone(),
        "textContent": block.content.clone(),
        "parent_id": block.parent_id.clone(),
        "parentId": block.parent_id.clone(),
        "order": block.order,
        "level": block.level,
        "checked": block.checked,
        "language": block.language.clone(),
        "marks": block.marks.clone(),
        "text_length": block.content.chars().count(),
        "textLength": block.content.chars().count(),
        "context": block_context(blocks, index),
    })
}

pub(super) fn block_matches_query(
    block: &BlockSnapshot,
    query: &serde_json::Map<String, serde_json::Value>,
) -> Result<bool, String> {
    if query.get("indent").is_some()
        || query.get("indent_gte").is_some()
        || query.get("indentGte").is_some()
        || query.get("indent_lte").is_some()
        || query.get("indentLte").is_some()
        || query.get("list_type").is_some()
        || query.get("listType").is_some()
    {
        return Err(
            "query_blocks indent and list_type filters require full block attribute projection"
                .to_string(),
        );
    }

    if let Some(expected) = query
        .get("block_type")
        .or_else(|| query.get("blockType"))
        .and_then(serde_json::Value::as_str)
    {
        if block.block_type != expected {
            return Ok(false);
        }
    }
    if let Some(expected) = query
        .get("heading_level")
        .or_else(|| query.get("headingLevel"))
        .and_then(serde_json::Value::as_i64)
    {
        if block.block_type != "heading" || block.level != Some(expected) {
            return Ok(false);
        }
    }
    if let Some(expected) = query.get("checked").and_then(serde_json::Value::as_bool) {
        if block.checked != Some(expected) {
            return Ok(false);
        }
    }
    if let Some(text) = query
        .get("text_contains")
        .or_else(|| query.get("textContains"))
        .and_then(serde_json::Value::as_str)
    {
        if !block.content.to_lowercase().contains(&text.to_lowercase()) {
            return Ok(false);
        }
    }

    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

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

    #[test]
    fn block_json_includes_context_and_hosted_aliases() {
        let blocks = vec![
            test_block("a", "paragraph", "One"),
            test_block("b", "heading", "Two"),
            test_block("c", "paragraph", "Three"),
        ];

        let value = block_json(&blocks[1], 1, &blocks);

        assert_eq!(value["block_id"], "b");
        assert_eq!(value["blockId"], "b");
        assert_eq!(value["block_type"], "heading");
        assert_eq!(value["blockType"], "heading");
        assert_eq!(value["text_length"], 3);
        assert_eq!(value["context"]["previousBlockId"], "a");
        assert_eq!(value["context"]["nextBlockId"], "c");
    }

    #[test]
    fn block_query_filters_match_supported_projection_fields() {
        let heading = test_block("h", "heading", "Semantic Architecture");
        let todo = test_block("todo", "todo", "Ship it");
        let paragraph = test_block("p", "paragraph", "plain text");

        let query = serde_json::json!({
            "blockType": "heading",
            "headingLevel": 2,
            "textContains": "semantic"
        });
        assert!(block_matches_query(&heading, query.as_object().unwrap()).unwrap());
        assert!(!block_matches_query(&paragraph, query.as_object().unwrap()).unwrap());

        let checked = serde_json::json!({ "checked": true });
        assert!(block_matches_query(&todo, checked.as_object().unwrap()).unwrap());

        let unsupported = serde_json::json!({ "indentGte": 1 });
        assert!(block_matches_query(&heading, unsupported.as_object().unwrap()).is_err());
    }
}

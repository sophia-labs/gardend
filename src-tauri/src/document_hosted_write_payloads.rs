use crate::json_utils::json_string;

pub(super) fn hosted_document_write_payload(
    document_id: &str,
    input: serde_json::Value,
) -> Result<serde_json::Value, String> {
    let title = json_string(input.get("title"))
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .ok_or_else(|| "title is required".to_string())?;
    let blocks = input
        .get("blocks")
        .filter(|value| value.is_array())
        .cloned()
        .unwrap_or_else(|| serde_json::json!([]));
    let tiptap_json = input.get("tiptapJson").or_else(|| input.get("tiptap_json"));
    if tiptap_json.is_some_and(|value| !value.is_object()) {
        return Err("tiptapJson must be an object".to_string());
    }
    let mut payload = serde_json::json!({
        "documentId": document_id,
        "title": title,
        "parentId": input.get("parentId").or_else(|| input.get("parent_id")).cloned().unwrap_or(serde_json::Value::Null),
        "order": input.get("order").cloned().unwrap_or(serde_json::Value::Null),
        "readOnly": input.get("readOnly").or_else(|| input.get("read_only")).cloned().unwrap_or(serde_json::Value::Bool(false)),
        "blocks": blocks,
        // A2 item 9: thread expectedRevision into the CRDT payload so the
        // save_document handler can apply the idempotency-aware increment.
        "expectedRevision": input.get("expectedRevision").or_else(|| input.get("expected_revision")).cloned().unwrap_or(serde_json::Value::Null),
    });
    if let Some(tiptap_json) = tiptap_json {
        payload["tiptapJson"] = tiptap_json.clone();
    }
    Ok(payload)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hosted_write_payload_normalizes_minimum_document_contract() {
        let payload = hosted_document_write_payload(
            "doc-a",
            serde_json::json!({
                "title": "  A Title  ",
                "parent_id": "folder-a",
                "read_only": true,
                "blocks": [{ "id": "b" }]
            }),
        )
        .unwrap();

        assert_eq!(payload["documentId"], "doc-a");
        assert_eq!(payload["title"], "A Title");
        assert_eq!(payload["parentId"], "folder-a");
        assert_eq!(payload["readOnly"], true);
        assert!(
            hosted_document_write_payload("doc-a", serde_json::json!({ "title": " " })).is_err()
        );
    }

    #[test]
    fn hosted_write_payload_preserves_tiptap_json() {
        let tiptap_json = serde_json::json!({
            "type": "doc",
            "content": [{
                "type": "paragraph",
                "attrs": { "data-block-id": "block-a" }
            }]
        });
        let payload = hosted_document_write_payload(
            "doc-a",
            serde_json::json!({
                "title": "A Title",
                "tiptapJson": tiptap_json,
            }),
        )
        .unwrap();

        assert_eq!(payload["tiptapJson"], tiptap_json);
    }
}

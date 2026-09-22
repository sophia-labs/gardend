pub(super) fn edit_comment_payload(
    arguments: &serde_json::Value,
    document_id: &str,
    comment_id: &str,
    action: &str,
) -> serde_json::Value {
    let mut payload = serde_json::json!({
        "documentId": document_id,
        "commentId": comment_id,
        "action": action,
        "text": arguments.get("text").cloned().unwrap_or(serde_json::Value::Null),
        "author": arguments.get("author").cloned().unwrap_or(serde_json::Value::Null),
        "blockId": arguments.get("blockId").or_else(|| arguments.get("block_id")).cloned().unwrap_or(serde_json::Value::Null),
        "find": arguments.get("find").cloned().unwrap_or(serde_json::Value::Null),
        "occurrence": arguments.get("occurrence").cloned().unwrap_or(serde_json::Value::Null),
        "quotedText": arguments.get("quotedText").or_else(|| arguments.get("quoted_text")).cloned().unwrap_or(serde_json::Value::Null),
        "resolved": arguments.get("resolved").cloned().unwrap_or(serde_json::Value::Null),
    });
    if let Some(object) = payload.as_object_mut() {
        object.retain(|_, value| !value.is_null());
    }
    payload
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn edit_comment_payload_strips_nulls_and_prefers_camel_aliases() {
        let payload = edit_comment_payload(
            &serde_json::json!({
                "text": "updated",
                "blockId": "camel-block",
                "block_id": "snake-block",
                "quoted_text": "quote",
                "resolved": false,
            }),
            "doc-a",
            "comment-a",
            "set",
        );

        assert_eq!(payload.get("documentId"), Some(&serde_json::json!("doc-a")));
        assert_eq!(
            payload.get("commentId"),
            Some(&serde_json::json!("comment-a"))
        );
        assert_eq!(
            payload.get("blockId"),
            Some(&serde_json::json!("camel-block"))
        );
        assert_eq!(payload.get("quotedText"), Some(&serde_json::json!("quote")));
        assert_eq!(payload.get("resolved"), Some(&serde_json::json!(false)));
        assert!(payload.get("author").is_none());
        assert!(payload.get("occurrence").is_none());
    }
}

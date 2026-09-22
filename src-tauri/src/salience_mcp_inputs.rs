use crate::{
    mcp_utils::{mcp_arg_string, mcp_required_document_id},
    salience_value_store::normalize_value_tags,
};

#[derive(Debug)]
pub(crate) struct LocalValueInputEntry {
    pub(crate) input_index: usize,
    pub(crate) document_id: String,
    pub(crate) block_id: String,
    pub(crate) importance: Option<i64>,
    pub(crate) valence: Option<i64>,
    pub(crate) tags: Vec<String>,
}

fn mcp_value_entry_string(value: &serde_json::Value, keys: &[&str]) -> Option<String> {
    keys.iter().find_map(|key| {
        value
            .get(*key)
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_string)
    })
}

fn mcp_value_entry_i64(value: &serde_json::Value, keys: &[&str]) -> Option<i64> {
    keys.iter()
        .find_map(|key| value.get(*key).and_then(serde_json::Value::as_i64))
}

fn mcp_value_entry_tags(value: &serde_json::Value) -> Vec<String> {
    value
        .get("tags")
        .map(|tags| match tags {
            serde_json::Value::Array(values) => values
                .iter()
                .filter_map(serde_json::Value::as_str)
                .map(str::to_string)
                .collect::<Vec<_>>(),
            serde_json::Value::String(value) => vec![value.to_string()],
            _ => Vec::new(),
        })
        .unwrap_or_default()
}

pub(crate) fn mcp_value_inputs(
    arguments: &serde_json::Value,
) -> Result<(bool, Vec<LocalValueInputEntry>), String> {
    if let Some(valuations) = arguments
        .get("valuations")
        .and_then(serde_json::Value::as_array)
    {
        if mcp_value_entry_string(arguments, &["document_id", "documentId"]).is_some()
            || mcp_value_entry_string(arguments, &["block_id", "blockId"]).is_some()
        {
            return Err("provide either document_id/block_id or valuations, not both".to_string());
        }
        if valuations.is_empty() {
            return Err("valuations list must not be empty".to_string());
        }
        let mut entries = Vec::new();
        for (input_index, value) in valuations.iter().enumerate() {
            let document_id =
                mcp_value_entry_string(value, &["document_id", "documentId"]).unwrap_or_default();
            let block_id =
                mcp_value_entry_string(value, &["block_id", "blockId"]).unwrap_or_default();
            let importance = mcp_value_entry_i64(value, &["importance"]);
            let valence = mcp_value_entry_i64(value, &["valence"]);
            let tags = normalize_value_tags(mcp_value_entry_tags(value));
            entries.push(LocalValueInputEntry {
                input_index,
                document_id,
                block_id,
                importance,
                valence,
                tags,
            });
        }
        return Ok((false, entries));
    }

    let document_id = mcp_required_document_id(arguments)?;
    let block_id = mcp_arg_string(arguments, &["block_id", "blockId"])
        .ok_or_else(|| "blockId is required".to_string())?;
    let importance = mcp_value_entry_i64(arguments, &["importance"]);
    let valence = mcp_value_entry_i64(arguments, &["valence"]);
    let tags = normalize_value_tags(mcp_value_entry_tags(arguments));
    if importance.is_none() && valence.is_none() && tags.is_empty() {
        return Err("importance, valence, or tags is required".to_string());
    }
    Ok((
        true,
        vec![LocalValueInputEntry {
            input_index: 0,
            document_id,
            block_id,
            importance,
            valence,
            tags,
        }],
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn value_tool_input_parser_separates_single_and_batch_modes() {
        let (single, entries) = mcp_value_inputs(&serde_json::json!({
            "documentId": "doc-a",
            "blockId": "block-a",
            "importance": 3,
            "tags": "#Decision",
        }))
        .unwrap();
        assert!(single);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].document_id, "doc-a");
        assert_eq!(entries[0].block_id, "block-a");
        assert_eq!(entries[0].importance, Some(3));
        assert_eq!(entries[0].tags, vec!["decision".to_string()]);

        let (single, entries) = mcp_value_inputs(&serde_json::json!({
            "valuations": [
                {"document_id": "doc-a", "block_id": "block-a", "valence": -2}
            ]
        }))
        .unwrap();
        assert!(!single);
        assert_eq!(entries[0].valence, Some(-2));

        assert!(mcp_value_inputs(&serde_json::json!({
            "document_id": "doc-a",
            "valuations": []
        }))
        .unwrap_err()
        .contains("either document_id/block_id or valuations"));
    }
}

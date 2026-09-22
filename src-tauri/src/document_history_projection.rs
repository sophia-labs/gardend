use crate::{document_projection_service::html_escape, document_service::BlockSnapshot};
use std::collections::{BTreeMap, BTreeSet};

fn snapshot_block_text_map(blocks: &[BlockSnapshot]) -> BTreeMap<String, String> {
    blocks
        .iter()
        .map(|block| (block.id.clone(), block.content.clone()))
        .collect()
}

pub(super) fn snapshot_diff_counts(
    old_blocks: &[BlockSnapshot],
    new_blocks: &[BlockSnapshot],
) -> (i64, i64, i64, i64, i64) {
    let old_map = snapshot_block_text_map(old_blocks);
    let new_map = snapshot_block_text_map(new_blocks);
    let old_ids = old_map.keys().cloned().collect::<BTreeSet<_>>();
    let new_ids = new_map.keys().cloned().collect::<BTreeSet<_>>();
    let added_ids = new_ids.difference(&old_ids).cloned().collect::<Vec<_>>();
    let removed_ids = old_ids.difference(&new_ids).cloned().collect::<Vec<_>>();
    let modified_ids = old_ids
        .intersection(&new_ids)
        .filter(|id| old_map.get(*id) != new_map.get(*id))
        .cloned()
        .collect::<Vec<_>>();

    let chars_added = added_ids
        .iter()
        .map(|id| {
            new_map
                .get(id)
                .map(|text| text.chars().count())
                .unwrap_or(0) as i64
        })
        .sum::<i64>()
        + modified_ids
            .iter()
            .map(|id| {
                let old_len = old_map
                    .get(id)
                    .map(|text| text.chars().count())
                    .unwrap_or(0) as i64;
                let new_len = new_map
                    .get(id)
                    .map(|text| text.chars().count())
                    .unwrap_or(0) as i64;
                (new_len - old_len).max(0)
            })
            .sum::<i64>();
    let chars_removed = removed_ids
        .iter()
        .map(|id| {
            old_map
                .get(id)
                .map(|text| text.chars().count())
                .unwrap_or(0) as i64
        })
        .sum::<i64>()
        + modified_ids
            .iter()
            .map(|id| {
                let old_len = old_map
                    .get(id)
                    .map(|text| text.chars().count())
                    .unwrap_or(0) as i64;
                let new_len = new_map
                    .get(id)
                    .map(|text| text.chars().count())
                    .unwrap_or(0) as i64;
                (old_len - new_len).max(0)
            })
            .sum::<i64>();

    (
        added_ids.len() as i64,
        removed_ids.len() as i64,
        modified_ids.len() as i64,
        chars_added,
        chars_removed,
    )
}

pub(super) fn snapshot_blocks_markdown(blocks: &[BlockSnapshot]) -> String {
    blocks
        .iter()
        .map(|block| match block.block_type.as_str() {
            "heading" => {
                let level = block.level.unwrap_or(1).clamp(1, 6) as usize;
                format!("{} {}", "#".repeat(level), block.content)
            }
            "codeBlock" | "code_block" | "code" => {
                let language = block.language.clone().unwrap_or_default();
                format!("```{language}\n{}\n```", block.content)
            }
            "blockquote" | "quote" => format!("> {}", block.content),
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
        .filter(|line| !line.trim().is_empty())
        .collect::<Vec<_>>()
        .join("\n\n")
}

pub(super) fn snapshot_blocks_html_fragment(blocks: &[BlockSnapshot]) -> String {
    blocks
        .iter()
        .map(|block| {
            let block_id = html_escape(&block.id);
            let text = html_escape(&block.content);
            match block.block_type.as_str() {
                "heading" => {
                    let level = block.level.unwrap_or(1).clamp(1, 6);
                    format!("<h{level} data-block-id=\"{block_id}\">{text}</h{level}>")
                }
                "code" | "codeBlock" | "code_block" => {
                    let language = block.language.clone().unwrap_or_default();
                    let class_name = if language.trim().is_empty() {
                        String::new()
                    } else {
                        format!(" class=\"language-{}\"", html_escape(language.trim()))
                    };
                    format!(
                        "<pre data-block-id=\"{block_id}\"><code{class_name}>{text}</code></pre>"
                    )
                }
                "blockquote" | "quote" => {
                    format!("<blockquote data-block-id=\"{block_id}\"><p>{text}</p></blockquote>")
                }
                "todo" => {
                    let checked = if block.checked.unwrap_or(false) {
                        " checked"
                    } else {
                        ""
                    };
                    format!(
                        "<p data-block-id=\"{block_id}\"><input type=\"checkbox\" disabled{checked}> {text}</p>"
                    )
                }
                "bullet" => format!("<ul data-block-id=\"{block_id}\"><li>{text}</li></ul>"),
                "numbered" => format!("<ol data-block-id=\"{block_id}\"><li>{text}</li></ol>"),
                "divider" => format!("<hr data-block-id=\"{block_id}\">"),
                _ => format!("<p data-block-id=\"{block_id}\">{text}</p>"),
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
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
    fn snapshot_diff_counts_track_added_removed_modified_text() {
        let old_blocks = vec![
            test_block("a", "paragraph", "hi"),
            test_block("b", "paragraph", "remove"),
        ];
        let new_blocks = vec![
            test_block("a", "paragraph", "hiya"),
            test_block("c", "paragraph", "new"),
        ];

        assert_eq!(
            snapshot_diff_counts(&old_blocks, &new_blocks),
            (1, 1, 1, 5, 6)
        );
    }

    #[test]
    fn snapshot_markdown_preserves_common_block_shapes() {
        let blocks = vec![
            test_block("h", "heading", "Title"),
            test_block("code", "codeBlock", "let x = 1;"),
            test_block("todo", "todo", "done"),
            test_block("empty", "paragraph", ""),
        ];

        assert_eq!(
            snapshot_blocks_markdown(&blocks),
            "## Title\n\n```rust\nlet x = 1;\n```\n\n- [x] done"
        );
    }
}

//! Comment anchoring tolerates how a quote was copied (2026-10-18). The
//! clerks of a workflow run copied quotes from `read_blocks` markdown and
//! elided their ends with `…`; the book's text has soft line wraps, curly
//! quotes and em dashes. Exact `find` missed 21 of 33 verified findings. A
//! quote now matches after both sides are normalized the way Shrubbery's
//! reader overlay normalizes them (`source-quote-annotations.ts`), and the
//! mark covers exactly the original characters. An explicit block confines
//! matching to that block.
//!
//! Real text: four consecutive paragraphs of chapter I of The Interpretation
//! of Dreams, as gardend imported the EPUB (`tests/fixtures/comment_anchoring`).
use super::builder::ydoc_from_tiptap_json;
use super::document_ops::edit_comment_in_doc;
use super::projection::ydoc_to_tiptap_json;
use serde_json::{json, Map, Value};
use yrs::{Doc, Transact};

const BOOK: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/fixtures/comment_anchoring/freud-ch1-blocks.json"
));

fn book() -> Doc {
    let fixture: Value = serde_json::from_str(BOOK).unwrap();
    ydoc_from_tiptap_json(&fixture["tiptapJson"])
}

fn set(doc: &Doc, comment_id: &str, block_id: Option<&str>, find: &str) -> Result<Value, String> {
    let mut payload = Map::new();
    payload.insert("text".into(), json!("a note"));
    payload.insert("find".into(), json!(find));
    if let Some(block_id) = block_id {
        payload.insert("blockId".into(), json!(block_id));
    }
    let mut txn = doc.transact_mut();
    edit_comment_in_doc(&mut txn, &payload, "set", comment_id, 1_792_000_000_000.0)
}

/// `(block id, the exact text the comment's mark covers)`, in document order.
fn marked(doc: &Doc, comment_id: &str) -> Vec<(String, String)> {
    let json = ydoc_to_tiptap_json(doc);
    let mut out: Vec<(String, String)> = Vec::new();
    for block in json["content"].as_array().unwrap() {
        let id = block["attrs"]["data-block-id"]
            .as_str()
            .or_else(|| block["attrs"]["blockId"].as_str())
            .unwrap_or_default()
            .to_string();
        let mut text = String::new();
        for node in block["content"].as_array().into_iter().flatten() {
            let ours = node["marks"].as_array().into_iter().flatten().any(|mark| {
                mark["type"] == "commentMark" && mark["attrs"]["commentId"] == comment_id
            });
            if ours {
                text.push_str(node["text"].as_str().unwrap_or_default());
            }
        }
        if !text.is_empty() {
            out.push((id, text));
        }
    }
    out
}

fn stored(doc: &Doc, comment_id: &str) -> Option<Value> {
    use yrs::types::ToJson;
    use yrs::{Map as _, ReadTxn};
    let txn = doc.transact();
    let comments = txn.get_map("comments")?;
    comments
        .get(&txn, comment_id)
        .map(|value| serde_json::to_value(value.to_json(&txn)).unwrap())
}

/// 1. The live miss, verbatim: soft wraps as `\n` and an elided end (`…`).
/// And the same quote with spaces where the book wraps.
#[test]
fn a_wrapped_elided_quote_anchors_on_its_exact_original_span() {
    let doc = book();
    let live = "The younger Strümpell,\nson of the philosopher whose writings on the subject have\nalready more than once served us…";
    let result = set(&doc, "c-live", Some("block-be1fe535"), live).expect("the live quote anchors");
    assert_eq!(result["blockId"], "block-be1fe535");
    assert_eq!(
        marked(&doc, "c-live"),
        vec![(
            "block-be1fe535".to_string(),
            "The younger Strümpell,\nson of the philosopher whose writings on the subject have\nalready more than once served us"
                .to_string()
        )]
    );
    set(
        &doc,
        "c-spaced",
        Some("block-be1fe535"),
        "The younger Strümpell, son of the philosopher whose writings",
    )
    .expect("spaces where the book wraps");
    assert_eq!(
        marked(&doc, "c-spaced"),
        vec![(
            "block-be1fe535".to_string(),
            "The younger Strümpell,\nson of the philosopher whose writings".to_string()
        )]
    );
}

/// 2. Straight quotes for curly ones, `--` for an em dash across an italic.
#[test]
fn typographic_quotes_and_dashes_match_their_plain_forms() {
    let doc = book();
    set(
        &doc,
        "c-double",
        Some("block-c01fe85b"),
        "\"Every indistinctly perceived noise gives rise to corresponding dream pictures",
    )
    .expect("straight double quotes");
    assert_eq!(
        marked(&doc, "c-double"),
        vec![(
            "block-c01fe85b".to_string(),
            "“Every indistinctly perceived noise gives\nrise to corresponding dream pictures"
                .to_string()
        )]
    );
    set(
        &doc,
        "c-single",
        Some("block-be1fe535"),
        "Strümpell's experiment",
    )
    .expect("straight apostrophe");
    assert_eq!(
        marked(&doc, "c-single"),
        vec![(
            "block-be1fe535".to_string(),
            "Strümpell’s experiment".to_string()
        )]
    );
    set(
        &doc,
        "c-dash",
        Some("block-be1fe535"),
        "The External Sensory Stimuli. -- The younger Strümpell",
    )
    .expect("a double hyphen for an em dash");
    assert_eq!(
        marked(&doc, "c-dash"),
        vec![(
            "block-be1fe535".to_string(),
            "The External Sensory Stimuli.—The younger Strümpell".to_string()
        )]
    );
}

/// An explicit block must never silently move a comment to another block.
#[test]
fn a_quote_outside_its_named_block_is_refused() {
    let doc = book();
    let before = ydoc_to_tiptap_json(&doc);
    let error = set(
        &doc,
        "c-next",
        Some("block-be1fe535"),
        "There are a great many stimuli of such nature, ranging from those that are unavoidable",
    )
    .expect_err("the quote belongs to another block");
    assert!(
        error.contains("text not found in block block-be1fe535"),
        "{error}"
    );
    assert!(stored(&doc, "c-next").is_none());
    assert_eq!(ydoc_to_tiptap_json(&doc), before);
}

#[test]
fn a_block_without_find_anchors_the_whole_block_and_missing_blocks_are_refused() {
    for key in ["blockId", "block_id"] {
        let doc = book();
        let mut payload = Map::new();
        payload.insert("text".into(), json!("whole block"));
        payload.insert(key.into(), json!("block-be1fe535"));
        let result = {
            let mut txn = doc.transact_mut();
            edit_comment_in_doc(&mut txn, &payload, "set", "c-block", 1.0).unwrap()
        };
        assert_eq!(result["blockId"], "block-be1fe535");
        assert!(result["anchored"].as_u64().unwrap() > 0);
        assert_eq!(
            marked(&doc, "c-block"),
            vec![("block-be1fe535".into(), block_text(&doc, "block-be1fe535"))]
        );
        payload.insert(key.into(), json!("missing"));
        let before = ydoc_to_tiptap_json(&doc);
        let error = {
            let mut txn = doc.transact_mut();
            edit_comment_in_doc(&mut txn, &payload, "set", "c-missing", 1.0).unwrap_err()
        };
        assert!(error.contains("Block not found"), "{error}");
        assert!(stored(&doc, "c-missing").is_none());
        assert_eq!(ydoc_to_tiptap_json(&doc), before);
    }
    let doc = book();
    assert!(set(&doc, "c-missing", Some("missing"), "Strümpell")
        .unwrap_err()
        .contains("Block not found"));
}

/// 4. A quote the book does not hold: reported, nothing written.
#[test]
fn an_absent_quote_is_reported_and_writes_nothing() {
    let doc = book();
    let before = ydoc_to_tiptap_json(&doc);
    let error = set(
        &doc,
        "c-absent",
        Some("block-be1fe535"),
        "The dreamer counts the sheep twice and wakes in Lisbon.",
    )
    .expect_err("not in the book");
    assert!(error.contains("text not found"), "{error}");
    assert!(stored(&doc, "c-absent").is_none());
    assert_eq!(ydoc_to_tiptap_json(&doc), before);
}

/// Two more copying habits, from run wfr-mvev25ht-e755un's verified
/// findings (verbatim, `tests/fixtures/comment_anchoring`): a quote copied
/// as a JSON-escaped string (a backslash and `n` where the book wraps), and a
/// sentence the clerk wrapped in quotation marks the book does not have.
const RUN_ITEMS: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/fixtures/comment_anchoring/run-wfr-mvev25ht-e755un-items.json"
));

fn run_item(n: &str) -> (String, String) {
    let items: Value = serde_json::from_str(RUN_ITEMS).unwrap();
    let item = &items["items"][n];
    (
        item["blockId"].as_str().unwrap().to_string(),
        item["quote"].as_str().unwrap().to_string(),
    )
}

fn block_text(doc: &Doc, block_id: &str) -> String {
    let json = ydoc_to_tiptap_json(doc);
    json["content"]
        .as_array()
        .unwrap()
        .iter()
        .find(|block| block["attrs"]["data-block-id"] == block_id)
        .map(|block| {
            block["content"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|node| node["text"].as_str())
                .collect()
        })
        .unwrap()
}

/// Item 1: `\` `n` (two characters) where the book has a newline.
#[test]
fn an_escaped_newline_in_a_quote_is_whitespace() {
    let doc = book();
    let (block_id, quote) = run_item("1");
    assert!(
        quote.contains("\\n"),
        "the item carries a literal backslash-n"
    );
    let original = quote.replace("\\n", "\n");
    assert!(block_text(&doc, &block_id).contains(&original));
    set(&doc, "c-item-1", Some(&block_id), &quote).expect("item 1 anchors");
    assert_eq!(marked(&doc, "c-item-1"), vec![(block_id, original)]);
}

/// Item 9: the sentence sits mid-paragraph; the clerk added “ ”.
#[test]
fn added_outer_quotation_marks_are_forgiven() {
    let doc = book();
    let (block_id, quote) = run_item("9");
    assert!(quote.starts_with('“') && quote.ends_with('”'));
    let original = quote
        .trim_start_matches('“')
        .trim_end_matches('”')
        .to_string();
    let text = block_text(&doc, &block_id);
    assert!(text.contains(&original) && !text.contains(&quote));
    set(&doc, "c-item-9", Some(&block_id), &quote).expect("item 9 anchors");
    assert_eq!(marked(&doc, "c-item-9"), vec![(block_id, original)]);
}

#[test]
fn moving_a_comment_replaces_marks_only_after_the_new_anchor_is_valid() {
    let doc = book();
    set(
        &doc,
        "c-move",
        Some("block-be1fe535"),
        "Strümpell’s experiment",
    )
    .unwrap();
    let before = marked(&doc, "c-move");
    assert!(set(
        &doc,
        "c-move",
        Some("block-c01fe85b"),
        "Strümpell’s experiment"
    )
    .is_err());
    assert_eq!(marked(&doc, "c-move"), before);
    assert_eq!(stored(&doc, "c-move").unwrap()["blockId"], "block-be1fe535");
    set(
        &doc,
        "c-move",
        Some("block-c01fe85b"),
        "Every indistinctly perceived noise",
    )
    .unwrap();
    assert_eq!(
        marked(&doc, "c-move"),
        vec![(
            "block-c01fe85b".into(),
            "Every indistinctly perceived noise".into()
        )]
    );
    assert_eq!(stored(&doc, "c-move").unwrap()["blockId"], "block-c01fe85b");
}

//! Composition-op tests for the M1.5 handlers ported in
//! src/crdt_engine/document_ops.rs and src/crdt_engine/workspace_ops.rs:
//! document.editComment, document.batchPrepare/batchRegister and their pure
//! helpers (occurrence offsets, batch folder-path expansion, comment
//! normalization, title chains), plus a RoomRegistry+tempdir lifecycle test
//! that drives batch prepare → register through a real hosted room.
//!
//! The pure helpers and real RoomRegistry are imported from Garden's library,
//! so this suite cannot silently drift behind hand-written handler stubs.

mod storage {
    use std::path::Path;

    pub(crate) fn write_json<T: serde::Serialize>(path: &Path, value: &T) -> Result<(), String> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|error| format!("mkdir {}: {error}", parent.display()))?;
        }
        let serialized = serde_json::to_vec_pretty(value)
            .map_err(|error| format!("serialize {}: {error}", path.display()))?;
        std::fs::write(path, serialized)
            .map_err(|error| format!("write {}: {error}", path.display()))
    }
}

mod ydoc_paths {
    use std::path::{Path, PathBuf};

    pub(crate) fn workspace_ydoc_state_path(graph_dir: &Path) -> PathBuf {
        graph_dir.join("ydocs/workspace").join("update-v1.bin")
    }
}

use garden_lib::crdt_engine::builder::ydoc_from_tiptap_json;
use garden_lib::crdt_engine::document_ops::{
    edit_comment_in_doc, encode_uri_component, filename_stem_title, normalize_comment_data,
    occurrence_offsets,
};
use garden_lib::crdt_engine::projection::ydoc_to_tiptap_json;
use garden_lib::crdt_engine::rooms::RoomRegistry;
use garden_lib::crdt_engine::workspace_ops::{
    batch_prepare_in_doc, batch_register_in_doc, expand_batch_folder_paths,
    materialize_workspace_snapshot_json, normalize_relative_path, parent_folder_path,
    parse_folder_map, path_basename, title_from_relative_path,
};
use serde_json::{json, Map, Value};
use yrs::Transact;

fn payload(value: Value) -> Map<String, Value> {
    value.as_object().cloned().expect("object payload")
}

// ---------------------------------------------------------------------------
// occurrenceOffsets (UTF-16 indexOf semantics)
// ---------------------------------------------------------------------------

#[test]
fn occurrence_offsets_match_js_index_of_semantics() {
    assert_eq!(occurrence_offsets("a b a b a", "a", 0.0), vec![0, 4, 8]);
    assert_eq!(occurrence_offsets("a b a b a", "a", 1.0), vec![0]);
    assert_eq!(occurrence_offsets("a b a b a", "a", 2.0), vec![4]);
    assert_eq!(occurrence_offsets("a b a b a", "a", -1.0), vec![8]);
    assert!(occurrence_offsets("a b a", "a", 5.0).is_empty());
    assert!(occurrence_offsets("abc", "", 1.0).is_empty());
    assert!(occurrence_offsets("abc", "zzz", 1.0).is_empty());
    // Non-overlapping scan: the cursor advances past each match.
    assert_eq!(occurrence_offsets("aaaa", "aa", 0.0), vec![0, 2]);
    // Offsets are UTF-16 code units: an emoji counts as two.
    assert_eq!(occurrence_offsets("😀x", "x", 1.0), vec![2]);
    assert_eq!(occurrence_offsets("😀😀", "😀", 2.0), vec![2]);
    // Math.max(1, Math.floor(occurrence)) clamps low values to the first hit.
    assert_eq!(occurrence_offsets("a b a", "a", -2.0), vec![0]);
    assert_eq!(occurrence_offsets("a b a", "a", 1.9), vec![0]);
}

// ---------------------------------------------------------------------------
// Batch folder-path helpers
// ---------------------------------------------------------------------------

#[test]
fn expand_batch_folder_paths_includes_ancestors_in_depth_order() {
    let input = vec![
        "a/b/c".to_string(),
        "z".to_string(),
        "a\\d".to_string(),
        " . ".to_string(),
        "../evil".to_string(),
    ];
    assert_eq!(
        expand_batch_folder_paths(&input),
        vec!["a", "z", "a/b", "a/d", "a/b/c"]
    );
    assert!(expand_batch_folder_paths(&[]).is_empty());
}

#[test]
fn relative_path_helpers_match_ts() {
    assert_eq!(
        normalize_relative_path("a\\b//c "),
        Some("a/b/c".to_string())
    );
    assert_eq!(normalize_relative_path("./x/."), Some("x".to_string()));
    assert_eq!(normalize_relative_path("a/../b"), None);
    assert_eq!(normalize_relative_path("  "), None);

    assert_eq!(parent_folder_path("a/b/c"), Some("a/b".to_string()));
    assert_eq!(parent_folder_path("a"), None);
    assert_eq!(parent_folder_path("../bad"), None);

    assert_eq!(path_basename("a/b/Note.md"), "Note.md");
    assert_eq!(path_basename(""), "Untitled Folder");

    assert_eq!(title_from_relative_path("docs/My Note.md"), "My Note");
    assert_eq!(title_from_relative_path("noext"), "noext");
    assert_eq!(title_from_relative_path(".hidden"), ".hidden");
}

#[test]
fn parse_folder_map_accepts_objects_and_json_strings() {
    let from_string = parse_folder_map(&json!(
        "{\"a/b\":\"f1\",\"bad/../x\":\"f2\",\"c\":null,\"\":\"f3\"}"
    ));
    assert_eq!(from_string.len(), 1);
    assert_eq!(from_string["a/b"], "f1");

    let from_object = parse_folder_map(&json!({ "x\\y": "f9" }));
    assert_eq!(from_object["x/y"], "f9");

    assert!(parse_folder_map(&json!(42)).is_empty());
    assert!(parse_folder_map(&json!("not json")).is_empty());
    assert!(parse_folder_map(&json!(["a"])).is_empty());
}

// ---------------------------------------------------------------------------
// normalizeCommentData merge semantics
// ---------------------------------------------------------------------------

#[test]
fn normalize_comment_data_applies_mcp_defaults() {
    let now = 1_700_000_000_000.0;
    let normalized = normalize_comment_data(
        &json!({ "text": null, "extra": "kept", "author_id": "u-1" }),
        now,
    );
    assert_eq!(normalized["text"], "");
    assert_eq!(normalized["author"], "MCP Agent");
    assert_eq!(normalized["authorId"], "u-1");
    assert_eq!(normalized["resolved"], false);
    assert_eq!(normalized["createdAt"], json!(1_700_000_000_000i64));
    assert_eq!(normalized["updatedAt"], json!(1_700_000_000_000i64));
    assert_eq!(normalized["extra"], "kept");

    // created_at passthrough wins over the now fallback; truthy resolved.
    let normalized = normalize_comment_data(&json!({ "created_at": 5, "resolved": 1 }), 9.0);
    assert_eq!(normalized["createdAt"], 5);
    assert_eq!(normalized["resolved"], true);
    assert_eq!(normalized["updatedAt"], 9);

    // Non-object / empty payloads pass through untouched (TS returns value).
    assert_eq!(
        normalize_comment_data(&json!("not-an-object"), 1.0),
        json!("not-an-object")
    );
    assert_eq!(normalize_comment_data(&json!({}), 1.0), json!({}));
}

// ---------------------------------------------------------------------------
// Title chains
// ---------------------------------------------------------------------------

#[test]
fn ingest_title_fallback_strips_extension_and_separators() {
    assert_eq!(filename_stem_title("my-paper_v2.pdf"), "my paper v2");
    assert_eq!(filename_stem_title("archive.tar.gz"), "archive.tar");
    assert_eq!(filename_stem_title("no_ext"), "no ext");
    assert_eq!(filename_stem_title("trailing."), "trailing.");
    assert_eq!(filename_stem_title(".hidden"), "");
    assert_eq!(filename_stem_title("a--b__c.md"), "a b c");
}

#[test]
fn encode_uri_component_matches_js_builtin() {
    assert_eq!(
        encode_uri_component("Final Report (v1).pdf"),
        "Final%20Report%20(v1).pdf"
    );
    assert_eq!(encode_uri_component("a+b&c=d"), "a%2Bb%26c%3Dd");
    assert_eq!(encode_uri_component("ümlaut"), "%C3%BCmlaut");
    assert_eq!(encode_uri_component("safe-_.!~*'()"), "safe-_.!~*'()");
}

// ---------------------------------------------------------------------------
// document.editComment in-transaction behavior against a real Y.Doc
// ---------------------------------------------------------------------------

fn comment_doc() -> yrs::Doc {
    ydoc_from_tiptap_json(&json!({
        "type": "doc",
        "content": [{
            "type": "paragraph",
            "attrs": { "data-block-id": "b1" },
            "content": [{ "type": "text", "text": "alpha beta alpha" }],
        }],
    }))
}

fn collect_comment_marks(node: &Value, found: &mut Vec<Value>) {
    if let Some(marks) = node.get("marks").and_then(Value::as_array) {
        for mark in marks {
            if mark.get("type").and_then(Value::as_str) == Some("commentMark") {
                found.push(mark.clone());
            }
        }
    }
    if let Some(content) = node.get("content").and_then(Value::as_array) {
        for child in content {
            collect_comment_marks(child, found);
        }
    }
}

#[test]
fn edit_comment_set_anchors_resolves_and_deletes() {
    let doc = comment_doc();
    let now = 1_700_000_000_000.0;

    // set: anchor at the 2nd occurrence of "alpha".
    let set_payload = payload(json!({
        "text": "important",
        "blockId": "b1",
        "find": "alpha",
        "occurrence": 2,
    }));
    let result = {
        let mut txn = doc.transact_mut();
        edit_comment_in_doc(&mut txn, &set_payload, "set", "c-1", now).expect("set comment")
    };
    assert_eq!(result["success"], true);
    assert_eq!(result["action"], "set");
    assert_eq!(result["commentId"], "c-1");
    assert_eq!(result["anchored"], 1);
    assert_eq!(result["blockId"], "b1");
    assert_eq!(result["quotedText"], "alpha", "quotedText defaults to find");
    assert_eq!(result["comment"]["text"], "important");
    assert_eq!(result["comment"]["author"], "MCP Agent");
    assert_eq!(result["comment"]["authorId"], "mcp-agent");
    assert_eq!(result["comment"]["resolved"], false);
    assert_eq!(result["comment"]["createdAt"], json!(1_700_000_000_000i64));

    // The commentMark formatting landed on exactly one range with the id.
    let tiptap = ydoc_to_tiptap_json(&doc);
    let mut marks = Vec::new();
    collect_comment_marks(&tiptap, &mut marks);
    assert_eq!(marks.len(), 1, "one marked range: {tiptap}");
    assert_eq!(marks[0]["attrs"]["commentId"], "c-1");

    // resolve: defaults to true, merges over the stored comment.
    let result = {
        let mut txn = doc.transact_mut();
        edit_comment_in_doc(
            &mut txn,
            &payload(json!({})),
            "resolve",
            "c-1",
            now + 1000.0,
        )
        .expect("resolve comment")
    };
    assert_eq!(result["resolved"], true);
    assert_eq!(result["comment"]["resolved"], true);
    assert_eq!(
        result["comment"]["text"], "important",
        "existing data merged"
    );
    assert_eq!(result["comment"]["updatedAt"], json!(1_700_000_001_000i64));

    // explicit resolved=false
    let result = {
        let mut txn = doc.transact_mut();
        edit_comment_in_doc(
            &mut txn,
            &payload(json!({ "resolved": false })),
            "resolve",
            "c-1",
            now + 2000.0,
        )
        .expect("unresolve comment")
    };
    assert_eq!(result["resolved"], false);

    // delete: removes the map entry and clears the mark formatting.
    let result = {
        let mut txn = doc.transact_mut();
        edit_comment_in_doc(&mut txn, &payload(json!({})), "delete", "c-1", now + 3000.0)
            .expect("delete comment")
    };
    assert_eq!(result["deleted"], true);
    assert_eq!(result["cleared"], 1);
    let tiptap = ydoc_to_tiptap_json(&doc);
    let mut marks = Vec::new();
    collect_comment_marks(&tiptap, &mut marks);
    assert!(marks.is_empty(), "comment marks cleared: {tiptap}");

    // deleting again: deleted=false, nothing cleared.
    let result = {
        let mut txn = doc.transact_mut();
        edit_comment_in_doc(&mut txn, &payload(json!({})), "delete", "c-1", now + 4000.0)
            .expect("re-delete comment")
    };
    assert_eq!(result["deleted"], false);
    assert_eq!(result["cleared"], 0);
}

#[test]
fn edit_comment_set_error_strings_are_verbatim() {
    let doc = comment_doc();
    let mut txn = doc.transact_mut();
    let error = edit_comment_in_doc(
        &mut txn,
        &payload(json!({ "text": "x", "blockId": "nope", "find": "alpha" })),
        "set",
        "c-9",
        1.0,
    )
    .unwrap_err();
    assert_eq!(error, "Block not found: nope");

    let error = edit_comment_in_doc(
        &mut txn,
        &payload(json!({ "text": "x", "blockId": "b1", "find": "zzz" })),
        "set",
        "c-9",
        1.0,
    )
    .unwrap_err();
    assert_eq!(error, "text not found in block b1: zzz");

    let error = edit_comment_in_doc(&mut txn, &payload(json!({})), "set", "c-9", 1.0).unwrap_err();
    assert_eq!(
        error,
        "document.editComment: text is required for action=set"
    );
}

#[test]
fn edit_comment_set_occurrence_zero_anchors_every_match() {
    let doc = comment_doc();
    let result = {
        let mut txn = doc.transact_mut();
        edit_comment_in_doc(
            &mut txn,
            &payload(json!({
                "text": "note",
                "blockId": "b1",
                "find": "alpha",
                "occurrence": 0,
                "quotedText": "custom quote",
                "author": "Vera",
                "authorId": "u-7",
            })),
            "set",
            "c-2",
            2.0,
        )
        .expect("set comment")
    };
    assert_eq!(result["anchored"], 2);
    assert_eq!(result["quotedText"], "custom quote");
    assert_eq!(result["comment"]["author"], "Vera");
    assert_eq!(result["comment"]["authorId"], "u-7");
    let tiptap = ydoc_to_tiptap_json(&doc);
    let mut marks = Vec::new();
    collect_comment_marks(&tiptap, &mut marks);
    assert_eq!(marks.len(), 2, "both occurrences marked: {tiptap}");
}

// ---------------------------------------------------------------------------
// document.batchPrepare / batchRegister lifecycle through a hosted room
// ---------------------------------------------------------------------------

#[test]
fn batch_prepare_register_lifecycle_via_room() {
    let profile_dir = std::env::temp_dir().join(format!("sophia-batch-{}", uuid::Uuid::new_v4()));
    let graph_dir = profile_dir.join("graphs").join("graph-batch");
    std::fs::create_dir_all(&graph_dir).expect("graph dir");
    storage::write_json(
        &graph_dir
            .join("documents")
            .join("doc-1")
            .join("document.json"),
        &json!({ "title": "  Paper One  " }),
    )
    .expect("doc-1 record");

    garden_lib::app_runtime::async_runtime::block_on(async {
        let registry = RoomRegistry::default();
        let state_path = ydoc_paths::workspace_ydoc_state_path(&graph_dir);
        let room = registry
            .get_or_create("workspace:graph-batch", state_path.clone())
            .await
            .expect("room");

        // peek (used by document.liveProjection): hosted key only.
        assert!(registry.peek("workspace:graph-batch").await.is_some());
        assert!(registry.peek("doc:graph-batch:doc-1").await.is_none());

        // prepare: creates ancestor folders from the injected folderIdMap.
        let prepare_payload = json!({
            "clientBatchKey": "batch-key-1",
            "batchId": "batch-1",
            "folders": ["research/papers"],
            "folderIdMap": { "research": "folder-r", "research/papers": "folder-rp" },
        });
        let prepared = room
            .update_doc(|_doc, txn| batch_prepare_in_doc(txn, &prepare_payload, "1700000000000"))
            .await
            .expect("batch prepare");
        assert_eq!(prepared["batchId"], "batch-1");
        assert_eq!(prepared["folderMap"]["research"], "folder-r");
        assert_eq!(prepared["folderMap"]["research/papers"], "folder-rp");

        // clientBatchKey idempotency: a replay with a fresh batchId returns
        // the cached batch and folder map.
        let replay_payload = json!({
            "clientBatchKey": "batch-key-1",
            "batchId": "batch-2",
            "folders": ["research/papers"],
            "folderIdMap": { "research": "folder-x", "research/papers": "folder-y" },
        });
        let replayed = room
            .update_doc(|_doc, txn| batch_prepare_in_doc(txn, &replay_payload, "1700000000500"))
            .await
            .expect("batch prepare replay");
        assert_eq!(replayed["batchId"], "batch-1");
        assert_eq!(replayed["folderMap"]["research/papers"], "folder-rp");

        // register: doc-1 has a record on disk; doc-ghost does not; the
        // third entry is missing its documentId.
        let register_payload = json!({
            "batchId": "batch-1",
            "documents": [
                {
                    "documentId": "doc-1",
                    "relativePath": "research/papers/paper-one.md",
                    "sourceFile": {
                        "storageKey": "local://documents/doc-1/original/paper.pdf",
                        "originalFilename": "paper.pdf",
                        "mimeType": "application/pdf",
                        "sizeBytes": 123,
                        "fileType": "pdf",
                    },
                },
                { "documentId": "doc-ghost", "relativePath": "research/papers/ghost.md" },
                { "relativePath": "research/none.md" },
            ],
        });
        let registered = room
            .update_doc(|_doc, txn| {
                batch_register_in_doc(txn, &graph_dir, &register_payload, "1700000001000")
            })
            .await
            .expect("batch register");
        assert_eq!(registered["registered"], 1);
        assert_eq!(registered["failed"], json!(["doc-ghost", "documents[2]"]));

        // unknown batch error (verbatim TS message)
        let error = room
            .update_doc(|_doc, txn| {
                batch_register_in_doc(txn, &graph_dir, &json!({ "batchId": "nope" }), "1")
            })
            .await
            .unwrap_err();
        assert_eq!(error, "upload batch not found: nope");
        assert!(
            registry.peek("workspace:graph-batch").await.is_none(),
            "a failed transaction evicts its possibly-mutated hot room"
        );
        let room = registry
            .get_or_create("workspace:graph-batch", state_path.clone())
            .await
            .expect("reopen after unknown-batch rejection");

        // prepare error strings (verbatim TS messages)
        let error = room
            .update_doc(|_doc, txn| batch_prepare_in_doc(txn, &json!({}), "1"))
            .await
            .unwrap_err();
        assert_eq!(error, "clientBatchKey is required");
        let room = registry
            .get_or_create("workspace:graph-batch", state_path.clone())
            .await
            .expect("reopen after missing-client-key rejection");
        let error = room
            .update_doc(|_doc, txn| {
                batch_prepare_in_doc(txn, &json!({ "clientBatchKey": "k2" }), "1")
            })
            .await
            .unwrap_err();
        assert_eq!(error, "document.batchPrepare: batchId is required");
        let room = registry
            .get_or_create("workspace:graph-batch", state_path.clone())
            .await
            .expect("reopen after missing-batch-id rejection");
        let error = room
            .update_doc(|_doc, txn| {
                batch_prepare_in_doc(
                    txn,
                    &json!({
                        "clientBatchKey": "k3",
                        "batchId": "b9",
                        "folders": ["x/y"],
                        "folderIdMap": { "x": "f-x" },
                    }),
                    "1",
                )
            })
            .await
            .unwrap_err();
        assert_eq!(
            error,
            "document.batchPrepare: folderId missing for path \"x/y\""
        );
        let room = registry
            .get_or_create("workspace:graph-batch", state_path.clone())
            .await
            .expect("reopen after missing-folder-id rejection");

        // Snapshot materialization shows the folders, the registered doc
        // (readOnly + full sf_* sourceFile), and the batch room state file.
        let snapshot = room
            .with_doc(|doc| materialize_workspace_snapshot_json("graph-batch", doc))
            .await
            .expect("materialize workspace snapshot");
        let folders = snapshot["folders"].as_array().expect("folders");
        let folder_ids: Vec<&str> = folders
            .iter()
            .filter_map(|folder| folder.get("id").and_then(Value::as_str))
            .collect();
        assert!(folder_ids.contains(&"folder-r"));
        assert!(folder_ids.contains(&"folder-rp"));
        let papers = folders
            .iter()
            .find(|folder| folder["id"] == "folder-rp")
            .expect("papers folder");
        assert_eq!(papers["parentId"], "folder-r");
        assert_eq!(papers["name"], "papers");

        let documents = snapshot["documents"].as_array().expect("documents");
        assert_eq!(documents.len(), 1);
        let document = &documents[0];
        assert_eq!(document["id"], "doc-1");
        assert_eq!(document["title"], "Paper One", "record title wins, trimmed");
        assert_eq!(document["parentId"], "folder-rp");
        assert_eq!(document["readOnly"], true);
        assert_eq!(
            document["sourceFile"]["sf_storageKey"],
            "local://documents/doc-1/original/paper.pdf"
        );
        assert_eq!(document["sourceFile"]["sf_originalFilename"], "paper.pdf");
        assert_eq!(document["sourceFile"]["sf_mimeType"], "application/pdf");
        assert_eq!(document["sourceFile"]["sf_sizeBytes"], 123);
        assert_eq!(document["sourceFile"]["sf_fileType"], "pdf");

        assert!(state_path.is_file(), "room persisted update-v1.bin");
    });
}

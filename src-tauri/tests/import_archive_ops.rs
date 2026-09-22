//! Focused tests for the pure core of src/crdt_engine/import_archive_ops.rs:
//! tar.gz parsing (manifest schema, document extraction, path safety, the
//! verbatim error strings ported from graph-archive-import.ts), RDF identity
//! rewriting, and workspace Y.Doc storage-key rewriting against a real yrs
//! Doc round-tripped through an encoded update — exactly how `apply` restores
//! the archived workspace.
//!
//! The pure archive surface is imported from Garden's real library rather
//! than recompiling the handler against test-only service shims.

use garden_lib::crdt_engine::import_archive_ops::{
    parse_graph_archive, rewrite_graph_archive_rdf, rewrite_graph_archive_workspace,
    GraphArchiveManifest,
};
use serde_json::{json, Value};
use yrs::updates::decoder::Decode;
use yrs::{
    Any, Doc, Map as YMap, MapPrelim, Out, ReadTxn, StateVector, Transact, Update, WriteTxn,
};

// ---------------------------------------------------------------------------
// Archive-building helpers
// ---------------------------------------------------------------------------

fn gzip(bytes: &[u8]) -> Vec<u8> {
    use flate2::{write::GzEncoder, Compression};
    use std::io::Write;
    let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
    encoder.write_all(bytes).expect("gzip write");
    encoder.finish().expect("gzip finish")
}

/// Build a real ustar tar.gz with the `tar` crate (the same family of bytes
/// the platform export route produces).
fn tar_gz(entries: &[(&str, &[u8])]) -> Vec<u8> {
    let mut builder = tar::Builder::new(Vec::new());
    for (path, data) in entries {
        let mut header = tar::Header::new_ustar();
        header.set_size(data.len() as u64);
        header.set_mode(0o644);
        header.set_cksum();
        builder
            .append_data(&mut header, path, *data)
            .expect("append tar entry");
    }
    gzip(&builder.into_inner().expect("finish tar"))
}

/// Hand-rolled single-entry tar for paths the `tar` crate refuses to write
/// (the malicious cases) and for corrupting header fields directly.
fn raw_tar_gz(name: &str, data: &[u8], size_field_override: Option<&str>) -> Vec<u8> {
    let mut header = [0u8; 512];
    header[..name.len()].copy_from_slice(name.as_bytes());
    let size_field = match size_field_override {
        Some(field) => field.to_string(),
        None => format!("{:011o}\0", data.len()),
    };
    header[124..124 + size_field.len()].copy_from_slice(size_field.as_bytes());
    header[156] = b'0';
    let mut tar_bytes = header.to_vec();
    tar_bytes.extend_from_slice(data);
    let pad = (512 - data.len() % 512) % 512;
    tar_bytes.extend(std::iter::repeat(0u8).take(pad));
    tar_bytes.extend([0u8; 1024]);
    gzip(&tar_bytes)
}

fn manifest_json() -> Value {
    json!({
        "version": 1,
        "format": "mnemosyne-graph-export",
        "source_user_id": "u-src",
        "source_graph_id": "g-src",
        "source_graph_title": "Source Graph",
        "source_graph_description": "A test graph",
        "includes_artifacts": true,
        "document_count": 2,
        "rdf_triple_count": 3,
        "files": {
            "rdf": "graph.nq",
            "workspace": "crdt/workspace.yjs",
            "documents_dir": "crdt/documents/",
            "artifacts_dir": "artifacts/"
        }
    })
}

fn manifest_bytes(manifest: &Value) -> Vec<u8> {
    serde_json::to_vec(manifest).expect("serialize manifest")
}

fn encode_full_state(doc: &Doc) -> Vec<u8> {
    let txn = doc.transact();
    txn.encode_state_as_update_v1(&StateVector::default())
}

/// A workspace Y.Doc shaped like the desktop app's: root `documents` and
/// `artifacts` Y.Maps holding per-entity Y.Maps with storage-key fields.
fn workspace_update() -> Vec<u8> {
    let doc = Doc::new();
    {
        let mut txn = doc.transact_mut();
        let documents = txn.get_or_insert_map("documents");
        let d1 = documents.insert(&mut txn, "doc-1", MapPrelim::default());
        d1.insert(&mut txn, "title", "Doc One");
        d1.insert(
            &mut txn,
            "sf_storageKey",
            "users/u-src/graphs/g-src/documents/doc-1/source.md",
        );
        let d2 = documents.insert(&mut txn, "doc-2", MapPrelim::default());
        d2.insert(&mut txn, "title", "Doc Two");
        let artifacts = txn.get_or_insert_map("artifacts");
        let a1 = artifacts.insert(&mut txn, "art-1", MapPrelim::default());
        a1.insert(&mut txn, "name", "paper.pdf");
        a1.insert(
            &mut txn,
            "storageKey",
            "users/u-src/graphs/g-src/artifacts/art-1/paper.pdf",
        );
        // Non-string storage key must be left untouched by the rewrite.
        let a2 = artifacts.insert(&mut txn, "art-2", MapPrelim::default());
        a2.insert(&mut txn, "storageKey", 42.0);
    }
    encode_full_state(&doc)
}

fn document_update(block_id: &str, text: &str) -> Vec<u8> {
    let doc = garden_lib::crdt_engine::builder::ydoc_from_tiptap_json(&json!({
        "type": "doc",
        "content": [{
            "type": "paragraph",
            "attrs": { "data-block-id": block_id },
            "content": [{ "type": "text", "text": text }],
        }],
    }));
    encode_full_state(&doc)
}

const SOURCE_NQUADS: &str = concat!(
    "<urn:mnemosyne:user:u-src:graph:g-src:doc:doc-1> <http://mnemosyne.dev/doc#title> \"Doc One\" <urn:mnemosyne:user:u-src:graph:g-src> .\n",
    "<urn:mnemosyne:user:u-src:graph:g-src:doc:doc-1> <http://mnemosyne.dev/doc#storage> \"users/u-src/graphs/g-src/documents/doc-1/source.md\" <urn:mnemosyne:user:u-src:graph:g-src> .\n",
    "<urn:x:unrelated> <urn:x:p> \"leave graphs/other/ and users/u-src2/ alone\" .\n",
);

fn full_archive() -> Vec<u8> {
    let manifest = manifest_bytes(&manifest_json());
    let workspace = workspace_update();
    let doc1 = document_update("block-1", "Hello from doc one");
    let doc2 = document_update("block-2", "Hello from doc two");
    tar_gz(&[
        ("manifest.json", manifest.as_slice()),
        ("graph.nq", SOURCE_NQUADS.as_bytes()),
        ("crdt/workspace.yjs", workspace.as_slice()),
        ("crdt/documents/doc-1.yjs", doc1.as_slice()),
        ("crdt/documents/doc-2.yjs", doc2.as_slice()),
    ])
}

fn test_manifest() -> GraphArchiveManifest {
    GraphArchiveManifest {
        source_user_id: "u-src".to_string(),
        source_graph_id: "g-src".to_string(),
        source_graph_title: Some("Source Graph".to_string()),
        source_graph_description: Some("A test graph".to_string()),
        includes_artifacts: true,
    }
}

fn nested_string(doc: &Doc, root: &str, child_key: &str, field: &str) -> Option<String> {
    let txn = doc.transact();
    let map = txn.get_map(root)?;
    let Out::YMap(child) = map.get(&txn, child_key)? else {
        return None;
    };
    match child.get(&txn, field)? {
        Out::Any(Any::String(s)) => Some(s.to_string()),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// parse_graph_archive — happy path
// ---------------------------------------------------------------------------

#[test]
fn parses_full_archive_manifest_workspace_documents_and_rdf() {
    let parsed = parse_graph_archive(&full_archive()).expect("parse archive");

    assert_eq!(parsed.manifest.source_user_id, "u-src");
    assert_eq!(parsed.manifest.source_graph_id, "g-src");
    assert_eq!(
        parsed.manifest.source_graph_title.as_deref(),
        Some("Source Graph")
    );
    assert_eq!(
        parsed.manifest.source_graph_description.as_deref(),
        Some("A test graph")
    );
    assert!(parsed.manifest.includes_artifacts);

    assert_eq!(parsed.rdf_n_quads, SOURCE_NQUADS);
    assert!(parsed.workspace_bytes.is_some());
    assert_eq!(
        parsed
            .documents
            .iter()
            .map(|(id, _)| id.as_str())
            .collect::<Vec<_>>(),
        vec!["doc-1", "doc-2"]
    );
    assert!(
        parsed.warnings.is_empty(),
        "warnings: {:?}",
        parsed.warnings
    );

    // The archived document bytes must be valid full updates: restore one and
    // project it the way `apply` does.
    let doc = Doc::new();
    let update = Update::decode_v1(&parsed.documents[0].1).expect("decode doc update");
    doc.transact_mut()
        .apply_update(update)
        .expect("apply doc update");
    let snapshot = garden_lib::crdt_engine::projection::materialize_ydoc(&doc, "doc-1");
    assert_eq!(snapshot.body.trim(), "Hello from doc one");
}

#[test]
fn collects_documents_in_nested_directories_and_warns_on_invalid_names() {
    let manifest = manifest_bytes(&manifest_json());
    let doc = document_update("block-1", "nested");
    let archive = tar_gz(&[
        ("manifest.json", manifest.as_slice()),
        ("graph.nq", b"".as_slice()),
        ("crdt/workspace.yjs", workspace_update().as_slice()),
        ("crdt/documents/sub/dir/doc-3.yjs", doc.as_slice()),
        ("crdt/documents/.yjs", doc.as_slice()),
    ]);
    let parsed = parse_graph_archive(&archive).expect("parse archive");
    assert_eq!(
        parsed
            .documents
            .iter()
            .map(|(id, _)| id.as_str())
            .collect::<Vec<_>>(),
        vec!["doc-3"]
    );
    assert!(parsed
        .warnings
        .contains(&"Skipped document with invalid archive path: crdt/documents/.yjs".to_string()));
}

#[test]
fn warns_when_workspace_or_documents_are_missing() {
    let manifest = manifest_bytes(&manifest_json());
    let archive = tar_gz(&[("manifest.json", manifest.as_slice())]);
    let parsed = parse_graph_archive(&archive).expect("parse archive");
    assert!(parsed.workspace_bytes.is_none());
    assert!(parsed.documents.is_empty());
    assert_eq!(
        parsed.warnings,
        vec![
            "Archive does not contain a workspace Y.Doc".to_string(),
            "Archive does not contain document Y.Docs".to_string(),
        ]
    );
    assert_eq!(parsed.rdf_n_quads, "");
}

// ---------------------------------------------------------------------------
// parse_graph_archive — verbatim error strings
// ---------------------------------------------------------------------------

#[test]
fn rejects_non_gzip_data_with_verbatim_error() {
    let error = parse_graph_archive(b"definitely not gzip").unwrap_err();
    assert_eq!(
        error,
        "Invalid or corrupt archive: expected gzip-compressed tar data"
    );
}

#[test]
fn rejects_archive_without_manifest() {
    let archive = tar_gz(&[("other.txt", b"hello".as_slice())]);
    assert_eq!(
        parse_graph_archive(&archive).unwrap_err(),
        "Cannot read manifest.json from archive"
    );
}

#[test]
fn rejects_unparsable_manifest() {
    let archive = tar_gz(&[("manifest.json", b"{not json".as_slice())]);
    assert_eq!(
        parse_graph_archive(&archive).unwrap_err(),
        "Cannot parse manifest.json from archive"
    );
}

#[test]
fn rejects_unsupported_manifest_version() {
    let mut manifest = manifest_json();
    manifest["version"] = json!(2);
    let archive = tar_gz(&[("manifest.json", manifest_bytes(&manifest).as_slice())]);
    assert_eq!(
        parse_graph_archive(&archive).unwrap_err(),
        "Unsupported manifest version: 2"
    );

    let mut manifest = manifest_json();
    manifest.as_object_mut().unwrap().remove("version");
    let archive = tar_gz(&[("manifest.json", manifest_bytes(&manifest).as_slice())]);
    assert_eq!(
        parse_graph_archive(&archive).unwrap_err(),
        "Unsupported manifest version: undefined"
    );
}

#[test]
fn rejects_unknown_archive_format() {
    let mut manifest = manifest_json();
    manifest["format"] = json!("zipgraph");
    let archive = tar_gz(&[("manifest.json", manifest_bytes(&manifest).as_slice())]);
    assert_eq!(
        parse_graph_archive(&archive).unwrap_err(),
        "Unknown archive format: zipgraph"
    );
}

#[test]
fn rejects_manifest_missing_source_identity() {
    let mut manifest = manifest_json();
    manifest["source_user_id"] = json!("");
    let archive = tar_gz(&[("manifest.json", manifest_bytes(&manifest).as_slice())]);
    assert_eq!(
        parse_graph_archive(&archive).unwrap_err(),
        "Archive manifest is missing source user or graph identity"
    );

    let mut manifest = manifest_json();
    manifest.as_object_mut().unwrap().remove("source_graph_id");
    let archive = tar_gz(&[("manifest.json", manifest_bytes(&manifest).as_slice())]);
    assert_eq!(
        parse_graph_archive(&archive).unwrap_err(),
        "Archive manifest is missing source user or graph identity"
    );
}

// ---------------------------------------------------------------------------
// tar safety
// ---------------------------------------------------------------------------

#[test]
fn rejects_parent_directory_traversal_paths() {
    let archive = raw_tar_gz("../evil.txt", b"x", None);
    assert_eq!(
        parse_graph_archive(&archive).unwrap_err(),
        "Unsafe path in archive: ../evil.txt"
    );

    let archive = raw_tar_gz("a/../b.txt", b"x", None);
    assert_eq!(
        parse_graph_archive(&archive).unwrap_err(),
        "Unsafe path in archive: a/../b.txt"
    );
}

#[test]
fn rejects_absolute_paths() {
    let archive = raw_tar_gz("/etc/passwd", b"x", None);
    assert_eq!(
        parse_graph_archive(&archive).unwrap_err(),
        "Unsafe path in archive: /etc/passwd"
    );
}

#[test]
fn rejects_truncated_entries() {
    // Header claims more data than the archive carries.
    let mut header = [0u8; 512];
    header[..9].copy_from_slice(b"short.txt");
    let size_field = format!("{:011o}\0", 4096);
    header[124..124 + size_field.len()].copy_from_slice(size_field.as_bytes());
    header[156] = b'0';
    let archive = gzip(&header);
    assert_eq!(
        parse_graph_archive(&archive).unwrap_err(),
        "Invalid or corrupt archive: truncated entry short.txt"
    );
}

#[test]
fn rejects_invalid_octal_size_fields() {
    let archive = raw_tar_gz("bad-size.txt", b"x", Some("zz"));
    assert_eq!(
        parse_graph_archive(&archive).unwrap_err(),
        "Invalid tar entry size: zz"
    );
}

// ---------------------------------------------------------------------------
// rewrite_graph_archive_rdf
// ---------------------------------------------------------------------------

#[test]
fn rewrites_rdf_graph_uri_and_storage_paths_exactly() {
    let rewritten = rewrite_graph_archive_rdf(SOURCE_NQUADS, &test_manifest(), "g-new");
    let expected = concat!(
        "<urn:mnemosyne:local:graph:g-new:doc:doc-1> <http://mnemosyne.dev/doc#title> \"Doc One\" <urn:mnemosyne:local:graph:g-new> .\n",
        "<urn:mnemosyne:local:graph:g-new:doc:doc-1> <http://mnemosyne.dev/doc#storage> \"users/default/graphs/g-new/documents/doc-1/source.md\" <urn:mnemosyne:local:graph:g-new> .\n",
        "<urn:x:unrelated> <urn:x:p> \"leave graphs/other/ and users/u-src2/ alone\" .\n",
    );
    assert_eq!(rewritten, expected);
}

// ---------------------------------------------------------------------------
// rewrite_graph_archive_workspace — real yrs Doc round-trip
// ---------------------------------------------------------------------------

#[test]
fn rewrites_workspace_storage_keys_on_restored_ydoc() {
    // Restore from the encoded update exactly the way `apply` does.
    let doc = Doc::new();
    let update = Update::decode_v1(&workspace_update()).expect("decode workspace update");
    doc.transact_mut()
        .apply_update(update)
        .expect("apply workspace update");

    rewrite_graph_archive_workspace(&doc, &test_manifest(), "g-new");

    assert_eq!(
        nested_string(&doc, "documents", "doc-1", "sf_storageKey").as_deref(),
        Some("users/default/graphs/g-new/documents/doc-1/source.md")
    );
    // Untouched fields stay untouched.
    assert_eq!(
        nested_string(&doc, "documents", "doc-1", "title").as_deref(),
        Some("Doc One")
    );
    assert_eq!(
        nested_string(&doc, "documents", "doc-2", "sf_storageKey"),
        None
    );
    assert_eq!(
        nested_string(&doc, "artifacts", "art-1", "storageKey").as_deref(),
        Some("users/default/graphs/g-new/artifacts/art-1/paper.pdf")
    );
    // Non-string storage keys are left alone.
    assert_eq!(
        nested_string(&doc, "artifacts", "art-2", "storageKey"),
        None
    );
    let txn = doc.transact();
    let artifacts = txn.get_map("artifacts").expect("artifacts map");
    let Some(Out::YMap(art2)) = artifacts.get(&txn, "art-2") else {
        panic!("art-2 missing");
    };
    assert!(matches!(
        art2.get(&txn, "storageKey"),
        Some(Out::Any(Any::Number(n))) if n == 42.0
    ));
}

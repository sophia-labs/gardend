//! Owner-bound preservation import. Source envelopes are integrity testimony,
//! never authentication, ACLs, native source ledgers or write receipts.
//!
//! All source bytes and cross-class references are checked before a target is
//! claimed. A completed operation is replayed by the parent completion ledger;
//! an incomplete claim is deliberately NOT reapplied over possibly edited
//! destination state. Its immutable archive remains available for recovery.
use super::*;
use crate::document_history_store::{LocalDocumentSnapshotMeta, LocalDocumentSnapshotPayload};
use oxigraph::model::{GraphName, NamedNode, NamedOrBlankNode, Quad, Term};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
#[cfg(feature = "desktop")]
use tauri::Manager;

#[cfg(test)]
#[path = "preservation_v2_tests.rs"]
mod tests;

const V21: &str = "cloud1-immutable-capture-v2.1";
const V22: &str = "cloud1-maintenance-capture-v2.2";
const V23: &str = "cloud1-legacy-snapshot-capture-v2.3";
const ROOT: &str = ".migration/preservation-v2";
const CLASSES: [&str; 7] = [
    "workspace",
    "documents",
    "originals",
    "rdf",
    "document-history",
    "graph-history",
    "deletions",
];
const MEMBER_LIMIT: usize = 512 * 1024 * 1024;
const MANIFEST_LIMIT: usize = 32 * 1024 * 1024;
const PAYLOAD_LIMIT: usize = 2 * 1024 * 1024 * 1024;
const HISTORY_EXPANSION_LIMIT: usize = 8 * 1024 * 1024 * 1024;
const MEMBER_COUNT: usize = 10_001;
const ORIGINAL_METADATA_CAPABILITY: &str = "original-source-metadata-evidence-v1";
const DATASET_CAPABILITY: &str = "retained-source-dataset-partitions-v1";
const DATASET_MEMBER: &str = "rdf/partitions.json";

type Members = BTreeMap<String, Vec<u8>>;
#[path = "derived_dataset.rs"]
mod derived_dataset;
fn require(ok: bool, reason: &str) -> Result<(), String> {
    if ok {
        Ok(())
    } else {
        Err(format!("preservation v2: {reason}"))
    }
}
fn string<'a>(value: &'a Value, key: &str) -> Result<&'a str, String> {
    value
        .get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| format!("preservation v2: missing string {key}"))
}
fn number(value: &Value, key: &str) -> Result<u64, String> {
    value
        .get(key)
        .and_then(Value::as_u64)
        .ok_or_else(|| format!("preservation v2: missing unsigned integer {key}"))
}
fn array<'a>(value: &'a Value, key: &str) -> Result<&'a Vec<Value>, String> {
    value
        .get(key)
        .and_then(Value::as_array)
        .ok_or_else(|| format!("preservation v2: missing array {key}"))
}
fn object<'a>(value: &'a Value, key: &str) -> Result<&'a JsonMap<String, Value>, String> {
    value
        .get(key)
        .and_then(Value::as_object)
        .ok_or_else(|| format!("preservation v2: missing object {key}"))
}

fn null_version_policy(
    members: &Members,
    manifest: &Value,
) -> Result<Option<(u64, BTreeSet<String>)>, String> {
    let sources = object(manifest, "sourceObjects")?;
    let semantic_has_null = sources
        .values()
        .any(|source| source.get("version_id").and_then(Value::as_str) == Some("null"));
    let custody = members
        .get("source-custody/index.json")
        .map(|bytes| parse_json(bytes))
        .transpose()?;
    let custody_has_null = match custody.as_ref() {
        Some(custody) => array(custody, "objects")?
            .iter()
            .any(|entry| entry.get("version_id").and_then(Value::as_str) == Some("null")),
        None => false,
    };
    if !semantic_has_null && !custody_has_null {
        return Ok(None);
    }
    require(
        manifest["transformation"] == V23
            && manifest["source"]["capture"]["backend"] == "cloud1-unfenced-persisted-storage"
            && manifest["source"]["capture"]["issuer"] == "cloud1-saved-version-collector",
        "literal null version requires v2.3 unfenced saved-state provenance",
    )?;
    let custody = custody.ok_or("preservation v2: missing member source-custody/index.json")?;
    require(
        custody["writerBoundary"] == "not-established"
            && custody["observationBoundary"] == "matching-version-inventories-not-a-writer-fence",
        "literal null version invented a writer fence",
    )?;
    let selection = object(&custody, "providerVersionSelection")?;
    require(
        selection["mode"] == "logical-saved-state-v1"
            && selection["exhaustiveProviderBackup"] == false
            && selection["nullVersionHandling"]
                == "explicit-version-id-null-read-and-fresh-byte-readback",
        "literal null version lacks logical saved-state selection",
    )?;
    let selected = number(
        &Value::Object(selection.clone()),
        "selectedNullVersionCount",
    )?;
    require(selected > 0, "literal null version count missing")?;
    let mut enabled_buckets = BTreeSet::new();
    for entry in array(&Value::Object(selection.clone()), "bucketVersioning")? {
        let bucket = string(entry, "bucket")?;
        require(
            !bucket.is_empty()
                && entry["status"] == "Enabled"
                && enabled_buckets.insert(bucket.to_string()),
            "invalid or duplicate null-version bucket testimony",
        )?;
    }
    require(
        !enabled_buckets.is_empty(),
        "null-version bucket testimony missing",
    )?;
    let readback = object(&custody, "nullVersionReadback")?;
    let first = readback
        .get("firstInventorySha256")
        .and_then(Value::as_str)
        .ok_or("preservation v2: missing string firstInventorySha256")?;
    let second = readback
        .get("secondInventorySha256")
        .and_then(Value::as_str)
        .ok_or("preservation v2: missing string secondInventorySha256")?;
    require_lower_sha256(first, "first null-version inventory hash")?;
    require_lower_sha256(second, "second null-version inventory hash")?;
    require(
        first == second && custody["inventorySha256"] == first,
        "null-version fresh inventory readback mismatch",
    )?;
    Ok(Some((selected, enabled_buckets)))
}
fn member<'a>(members: &'a Members, path: &str) -> Result<&'a [u8], String> {
    members
        .get(path)
        .map(Vec::as_slice)
        .ok_or_else(|| format!("preservation v2: missing member {path}"))
}
fn json_bytes(value: &impl Serialize) -> Result<Vec<u8>, String> {
    serde_json::to_vec(value).map_err(|e| e.to_string())
}
fn safe_path(path: &str) -> Result<(), String> {
    // ASCII is an explicit supported subset, not a Unicode normalization or
    // a case-insensitive filesystem-dependent overwrite policy.
    require(
        !path.is_empty()
            && path.len() <= 240
            && path.is_ascii()
            && !path.contains('\\')
            && !path.bytes().any(|c| c < 32 || c == 127)
            && path.split('/').all(|s| !matches!(s, "" | "." | "..")),
        "unsafe or unsupported member path",
    )
}
fn source_id(id: &str) -> Result<(), String> {
    crate::ids::validate_local_id(id, "preserved identity")?;
    require(
        id.len() <= 200
            && id.is_ascii()
            && id
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || b"_.:@-".contains(&c)),
        "unsupported source identity",
    )
}

// serde_json's default object reader is last-wins. Integrity envelopes must
// reject duplicate keys at every depth instead, including embedded descriptors.
struct StrictJson(Value);
impl<'de> Deserialize<'de> for StrictJson {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct Visitor;
        impl<'de> serde::de::Visitor<'de> for Visitor {
            type Value = StrictJson;
            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                f.write_str("JSON without duplicate object keys")
            }
            fn visit_bool<E: serde::de::Error>(self, v: bool) -> Result<Self::Value, E> {
                Ok(StrictJson(json!(v)))
            }
            fn visit_i64<E: serde::de::Error>(self, v: i64) -> Result<Self::Value, E> {
                Ok(StrictJson(json!(v)))
            }
            fn visit_u64<E: serde::de::Error>(self, v: u64) -> Result<Self::Value, E> {
                Ok(StrictJson(json!(v)))
            }
            fn visit_f64<E: serde::de::Error>(self, v: f64) -> Result<Self::Value, E> {
                serde_json::Number::from_f64(v)
                    .map(|n| StrictJson(Value::Number(n)))
                    .ok_or_else(|| E::custom("nonfinite number"))
            }
            fn visit_str<E: serde::de::Error>(self, v: &str) -> Result<Self::Value, E> {
                Ok(StrictJson(json!(v)))
            }
            fn visit_string<E: serde::de::Error>(self, v: String) -> Result<Self::Value, E> {
                Ok(StrictJson(json!(v)))
            }
            fn visit_none<E: serde::de::Error>(self) -> Result<Self::Value, E> {
                Ok(StrictJson(Value::Null))
            }
            fn visit_unit<E: serde::de::Error>(self) -> Result<Self::Value, E> {
                Ok(StrictJson(Value::Null))
            }
            fn visit_seq<A: serde::de::SeqAccess<'de>>(
                self,
                mut a: A,
            ) -> Result<Self::Value, A::Error> {
                let mut values = Vec::new();
                while let Some(StrictJson(v)) = a.next_element()? {
                    values.push(v);
                }
                Ok(StrictJson(Value::Array(values)))
            }
            fn visit_map<A: serde::de::MapAccess<'de>>(
                self,
                mut a: A,
            ) -> Result<Self::Value, A::Error> {
                let mut map = JsonMap::new();
                while let Some(key) = a.next_key::<String>()? {
                    if map.contains_key(&key) {
                        return Err(serde::de::Error::custom("duplicate JSON key"));
                    }
                    map.insert(key, a.next_value::<StrictJson>()?.0);
                }
                Ok(StrictJson(Value::Object(map)))
            }
        }
        d.deserialize_any(Visitor)
    }
}
fn parse_json(bytes: &[u8]) -> Result<Value, String> {
    serde_json::from_slice::<StrictJson>(bytes)
        .map(|v| v.0)
        .map_err(|e| format!("preservation JSON: {e}"))
}
fn octal(bytes: &[u8]) -> Result<usize, String> {
    require(
        bytes.iter().all(|c| matches!(c, b'0'..=b'7' | b' ' | 0)),
        "unsupported tar numeric encoding",
    )?;
    let text = std::str::from_utf8(bytes)
        .map_err(|e| e.to_string())?
        .trim_matches(['\0', ' ']);
    usize::from_str_radix(if text.is_empty() { "0" } else { text }, 8).map_err(|e| e.to_string())
}
fn tar_name(bytes: &[u8]) -> Result<&str, String> {
    let end = bytes.iter().position(|c| *c == 0).unwrap_or(bytes.len());
    require(
        bytes[end..].iter().all(|c| *c == 0),
        "tar name has bytes after terminator",
    )?;
    std::str::from_utf8(&bytes[..end]).map_err(|e| e.to_string())
}
fn unpack(bytes: &[u8]) -> Result<Members, String> {
    require(
        bytes.len() <= MAX_EXTRACTED_SIZE,
        "compressed archive exceeds budget",
    )?;
    let max = PAYLOAD_LIMIT + MANIFEST_LIMIT + MEMBER_COUNT * 2048 + MAX_TAR_TRAILING_BYTES;
    let mut decoder = flate2::bufread::GzDecoder::new(bytes);
    let mut tar = Vec::new();
    decoder
        .by_ref()
        .take(max as u64 + 1)
        .read_to_end(&mut tar)
        .map_err(|e| format!("preservation gzip: {e}"))?;
    require(
        tar.len() <= max && decoder.into_inner().is_empty(),
        "oversized or multiple/trailing gzip members",
    )?;
    let mut members = Members::new();
    let mut folded = BTreeSet::new();
    let mut offset = 0usize;
    let mut total = 0usize;
    let mut long_name: Option<String> = None;
    loop {
        require(offset + 1024 <= tar.len(), "unterminated tar archive")?;
        let header = &tar[offset..offset + 512];
        if header.iter().all(|c| *c == 0) {
            require(long_name.is_none(), "orphan GNU long name")?;
            require(
                tar.len() - offset <= MAX_TAR_TRAILING_BYTES
                    && tar[offset..].iter().all(|c| *c == 0),
                "nonzero or excessive tar terminal tail",
            )?;
            break;
        }
        let sum: usize = header
            .iter()
            .enumerate()
            .map(|(i, c)| {
                if (148..156).contains(&i) {
                    32
                } else {
                    *c as usize
                }
            })
            .sum();
        require(
            sum == octal(&header[148..156])?,
            "tar header checksum mismatch",
        )?;
        let gnu=&header[257..265]==b"ustar  \0";
        let ustar=&header[257..263]==b"ustar\0" && &header[263..265]==b"00";
        require((gnu || ustar) && header[157..257].iter().all(|c|*c==0),
            "only regular USTAR/GNU members are supported")?;
        let name=tar_name(&header[..100])?;
        let size=octal(&header[124..136])?;
        if gnu && header[156]==b'L' {
            require(long_name.is_none() && name=="././@LongLink" && (102..=241).contains(&size),
                "invalid or repeated GNU long name")?;
            let start=offset+512;
            require(start+512<=tar.len() && tar[start+size-1]==0
                && tar[start+size..start+512].iter().all(|c|*c==0), "GNU long name bounds/padding")?;
            let path=std::str::from_utf8(&tar[start..start+size-1]).map_err(|e|e.to_string())?;
            safe_path(path)?;
            long_name=Some(path.to_string());
            offset=start+512;
            continue;
        }
        require(matches!(header[156],0|b'0'), "only regular archive members are supported")?;
        let path=if let Some(path)=long_name.take() {
            require(gnu && name.as_bytes()==&path.as_bytes()[..100], "GNU long name/header mismatch")?;
            path
        } else if gnu {
            name.to_string()
        } else {
            let prefix=tar_name(&header[345..500])?;
            if prefix.is_empty() {name.to_string()} else {format!("{prefix}/{name}")}
        };
        safe_path(&path)?;
        require(
            folded.insert(path.to_ascii_lowercase()),
            "duplicate or case-colliding archive member",
        )?;
        require(
            !path
                .split('/')
                .any(|s| matches!(s, "text-owner.json" | "artifact-text-operations")),
            "native authority member refused",
        )?;
        let size = octal(&header[124..136])?;
        require(
            size <= if path == "manifest.json" {
                MANIFEST_LIMIT
            } else {
                MEMBER_LIMIT
            },
            "archive member exceeds budget",
        )?;
        if path != "manifest.json" {
            total = total.checked_add(size).ok_or("archive size overflow")?;
        }
        require(
            total <= PAYLOAD_LIMIT && members.len() < MEMBER_COUNT,
            "archive aggregate budget exceeded",
        )?;
        let padded = size.checked_add(511).ok_or("archive size overflow")? / 512 * 512;
        let start = offset + 512;
        require(start + padded <= tar.len(), "truncated tar member")?;
        require(
            tar[start + size..start + padded].iter().all(|c| *c == 0),
            "nonzero member padding",
        )?;
        members.insert(path, tar[start..start + size].to_vec());
        offset = start + padded;
    }
    for path in members.keys() {
        let mut parent = Path::new(path).parent();
        while let Some(p) = parent {
            require(
                !folded.contains(&p.to_string_lossy().to_ascii_lowercase()),
                "file/directory path collision",
            )?;
            parent = p.parent();
        }
    }
    Ok(members)
}

struct Original {
    kind: String,
    id: String,
    filename: String,
    mime: String,
    member: String,
    provenance: Value,
}
struct History {
    meta: LocalDocumentSnapshotMeta,
    payload: LocalDocumentSnapshotPayload,
}
struct GraphHistory {
    manifest: crate::time_travel_types::RestorePointManifest,
    workspace: Vec<u8>,
    snapshot: Value,
    documents: Vec<(String, Arc<[u8]>)>,
    payloads: Vec<GraphHistoryPayload>,
    workspace_only_document_ids: Vec<String>,
    storage_only_document_ids: Vec<String>,
    id_title_fallbacks: Vec<String>,
}

struct GraphHistoryContent {
    bytes: Arc<[u8]>,
    blocks: Vec<crate::document_types::BlockSnapshot>,
    tiptap_xml: String,
    char_count: u64,
}
type HistoryProjectionCache = BTreeMap<(String, String), Arc<GraphHistoryContent>>;

fn history_projection(cache: &mut HistoryProjectionCache, bytes: &[u8], doc_id: &str)
    -> Result<Arc<GraphHistoryContent>, String> {
    let key = (doc_id.to_string(), archive_sha256(bytes));
    if let Some(content) = cache.get(&key) {
        require(content.bytes.as_ref() == bytes, "historical projection digest collision")?;
        return Ok(Arc::clone(content));
    }
    let doc = parsed_doc(bytes, "historical document")?;
    let projection = super::super::projection::materialize_ydoc(&doc, doc_id);
    let content = Arc::new(GraphHistoryContent {
        bytes: Arc::from(bytes),
        blocks: serde_json::from_value(projection.blocks_json).map_err(|e| e.to_string())?,
        tiptap_xml: super::super::projection::ydoc_to_tiptap_xml(&doc),
        char_count: projection.body.chars().count() as u64,
    });
    cache.insert(key, Arc::clone(&content));
    Ok(content)
}

struct GraphHistoryPayload {
    snapshot_id: String,
    graph_id: String,
    document_id: String,
    title: String,
    created_at: String,
    content: Arc<GraphHistoryContent>,
}
impl GraphHistoryPayload {
    fn native(&self) -> LocalDocumentSnapshotPayload {
        LocalDocumentSnapshotPayload {
            snapshot_id: self.snapshot_id.clone(), graph_id: self.graph_id.clone(),
            document_id: self.document_id.clone(), title: self.title.clone(),
            created_at: self.created_at.clone(), blocks: self.content.blocks.clone(),
            tiptap_xml: self.content.tiptap_xml.clone(),
        }
    }
}

fn graph_history_disposition(point: &GraphHistory) -> Value {
    json!({"snapshotId":point.manifest.restore_point_id,
        "nativeSchemaVersion":point.manifest.schema_version,
        "restoreSupported":point.manifest.schema_version>=2,
        "reason":if point.manifest.schema_version>=2 {
            "exact historical workspace/document membership with all referenced bytes"
        } else {
            "historical workspace/document membership differs; referenced bytes retained read-only"
        },
        "workspaceOnlyDocumentIds":point.workspace_only_document_ids,
        "storageOnlyDocumentIds":point.storage_only_document_ids,
        "displayTitleFallbacks":point.id_title_fallbacks.iter().map(|id|json!({
            "documentId":id,"displayTitle":id,"sourceTitle":null,
            "reason":"document absent from historical workspace"})).collect::<Vec<_>>()})
}
struct Prepared {
    members: Members,
    manifest: Value,
    parsed: ParsedGraphArchive,
    workspace: Vec<u8>,
    snapshot: Value,
    originals: Vec<Original>,
    history: Vec<History>,
    graph_history: Vec<GraphHistory>,
    deletions: Vec<String>,
    rdf: String,
    graph_mapping: Value,
    dataset_partitions: Value,
    timestamp_normalization: Value,
    derived_disposition: Value,
    content_parity_disposition: Value,
    rdf_accounting: Value,
    metadata: BTreeMap<String, String>,
    regenerated: usize,
    unavailable_bodies: Vec<Value>,
    unavailable_history: Vec<Value>,
    history_availability: Value,
    history_disposition: Value,
}

fn class_paths(manifest: &Value, class: &str) -> Result<BTreeSet<String>, String> {
    array(&manifest["classes"], class)?
        .iter()
        .map(|v| {
            v.as_str()
                .map(str::to_string)
                .ok_or_else(|| "preservation class path is not a string".to_string())
        })
        .collect()
}
fn exact_class(manifest: &Value, class: &str, expected: BTreeSet<String>) -> Result<(), String> {
    require(
        class_paths(manifest, class)? == expected,
        &format!("unindexed or missing {class} member"),
    )
}
fn set(paths: &[&str]) -> BTreeSet<String> {
    paths.iter().map(|p| p.to_string()).collect()
}
fn legacy_completeness() -> Value {
    json!({"scope":"persisted-storage-only","acknowledgedTail":"unknown","processMemory":"not-captured"})
}

fn validate_envelope(
    members: &Members,
    manifest: &Value,
    operation: &CrdtOperation,
    bytes: &[u8],
) -> Result<(), String> {
    require(
        manifest["format"] == "mnemosyne-preservation-bundle"
            && manifest["version"].as_u64() == Some(2),
        "unsupported manifest format/version",
    )?;
    let transformation = string(manifest, "transformation")?;
    require(
        matches!(transformation, V21 | V22 | V23),
        "unsupported preservation transformation",
    )?;
    let source = &manifest["source"];
    let user = string(source, "userId")?;
    let graph = string(source, "graphId")?;
    source_id(user)?;
    source_id(graph)?;
    require(
        operation.payload["sourceUserId"] == user
            && operation.payload["sourceGraphId"] == graph
            && operation.graph_id == graph,
        "source owner/graph identity mismatch",
    )?;
    require(
        operation.payload["archiveSha256"] == archive_sha256(bytes),
        "archive hash mismatch",
    )?;
    // Same contract as v1: planDigest names the caller's reviewed plan. The
    // native envelope hash binds EVERY supplied semantic parameter plus that
    // digest and the actual destination incarnation, independent of a caller's
    // JSON ordering. It is not a signature or independent cut attestation.
    require_lower_sha256(string(&operation.payload, "planDigest")?, "planDigest")?;
    require(
        manifest["trust"]
            == json!({"dataIntegrityOnly":true,"targetAuthorization":"not-conferred"}),
        "source cannot confer destination authority",
    )?;
    require(
        manifest["excludedAuthority"]
            == json!([
                "credentials",
                "ACL-grants",
                "text-owner",
                "operation-receipts",
                "derived-projection-pdf-source"
            ]),
        "unknown authority disposition",
    )?;
    let mut supported = set(&CLASSES);
    if transformation != V21 {
        supported.insert("source-custody".into());
    }
    require(
        object(manifest, "classes")?
            .keys()
            .cloned()
            .collect::<BTreeSet<_>>()
            == supported,
        "unsupported or missing class",
    )?;
    let required = array(manifest, "requiredCapabilities")?;
    let mut supported_capabilities = supported.clone();
    if members.contains_key(DATASET_MEMBER) {
        require(transformation == V23, "dataset partitions require v2.3")?;
        supported_capabilities.insert(DATASET_CAPABILITY.into());
    }
    let original_index = parse_json(member(members, "originals/index.json")?)?;
    let has_original_metadata_evidence = original_index.as_array()
        .ok_or("original index must be an array")?.iter()
        .any(|entry| entry.get("sourceMetadataEvidence").is_some());
    if has_original_metadata_evidence {
        require(transformation == V23, "original metadata evidence requires v2.3")?;
        supported_capabilities.insert(ORIGINAL_METADATA_CAPABILITY.into());
    }
    let required_set = required
        .iter()
        .map(|v| {
            v.as_str()
                .map(str::to_string)
                .ok_or("invalid required capability")
        })
        .collect::<Result<BTreeSet<_>, _>>()?;
    require(
        required_set == supported_capabilities && required.len() == supported_capabilities.len(),
        "required capabilities do not match classes",
    )?;
    let inventory = object(manifest, "members")?;
    let sources = object(manifest, "sourceObjects")?;
    let null_policy = null_version_policy(members, manifest)?;
    let actual: BTreeSet<_> = members
        .keys()
        .filter(|p| p.as_str() != "manifest.json")
        .cloned()
        .collect();
    require(
        inventory.keys().cloned().collect::<BTreeSet<_>>() == actual
            && sources.keys().cloned().collect::<BTreeSet<_>>() == actual,
        "archive inventory is not closed",
    )?;
    let descriptor_text = string(manifest, "captureDescriptorCanonicalJson")?;
    let descriptor = parse_json(descriptor_text.as_bytes())?;
    let capture = &source["capture"];
    require(
        capture["descriptorSha256"] == archive_sha256(descriptor_text.as_bytes()),
        "capture descriptor hash mismatch",
    )?;
    for (native, wire) in [
        ("capture_id", "id"),
        ("issuer", "issuer"),
        ("source_backend", "backend"),
        ("source_account_id", "storageAccountId"),
        ("source_revision", "sourceRevision"),
        ("journal_boundaries", "journalBoundaries"),
    ] {
        require(
            descriptor[native] == capture[wire] && !descriptor[native].is_null(),
            "capture descriptor testimony mismatch",
        )?;
    }
    if transformation == V23 {
        require(
            descriptor["snapshot_boundaries"] == capture["snapshotBoundaries"]
                && capture["snapshotBoundaries"].is_array()
                && capture["journalBoundaries"] == json!([])
                && capture["sourceCompleteness"] == legacy_completeness(),
            "legacy snapshot boundary or source completeness mismatch",
        )?;
    } else {
        require(
            capture["sourceCompleteness"].is_null()
                && (capture["snapshotBoundaries"].is_null()
                    || capture["snapshotBoundaries"] == json!([]))
                && (descriptor["snapshot_boundaries"].is_null()
                    || descriptor["snapshot_boundaries"] == json!([])),
            "legacy snapshot testimony requires explicit v2.3 transformation",
        )?;
    }
    require(
        descriptor["user_id"] == user
            && descriptor["graph_id"] == graph
            && descriptor["unresolved"] == json!([])
            && descriptor
                .get("transformation")
                .and_then(Value::as_str)
                .unwrap_or(V21)
                == transformation
            && descriptor["named_graphs"] == manifest["namedGraphs"]
            && descriptor["document_history_records"] == manifest["sourceRecords"],
        "descriptor identity, records or disposition mismatch",
    )?;
    let coverage = object(capture, "coverage")?;
    require(
        coverage.len() == supported.len() + 2
            && capture["coverage"]["inline-images"] == "included-in-originals"
            && capture["coverage"]["native-html-authority"] == "absent",
        "coverage incomplete",
    )?;
    let mut descriptor_coverage = JsonMap::new();
    for pair in array(&descriptor, "coverage")? {
        let pair = pair.as_array().ok_or("invalid descriptor coverage")?;
        require(pair.len() == 2, "invalid coverage pair")?;
        let name = pair[0].as_str().ok_or("invalid coverage name")?;
        require(
            descriptor_coverage
                .insert(name.into(), pair[1].clone())
                .is_none(),
            "duplicate coverage",
        )?;
    }
    require(
        &descriptor_coverage == coverage,
        "coverage descriptor mismatch",
    )?;
    let mut described = BTreeMap::new();
    for entry in array(&descriptor, "members")? {
        let path = string(entry, "path")?;
        require(
            described.insert(path.to_string(), entry).is_none(),
            "duplicate descriptor member",
        )?;
    }
    require(
        described.keys().cloned().collect::<BTreeSet<_>>() == actual,
        "descriptor inventory incomplete",
    )?;
    let mut classified = BTreeSet::new();
    let prefix = format!("users/{user}/graphs/{graph}/");
    for class in supported {
        let paths = class_paths(manifest, &class)?;
        require(
            paths.len() == array(&manifest["classes"], &class)?.len()
                && (class == "documents" || !paths.is_empty()),
            "duplicate or absent class paths",
        )?;
        require(
            capture["coverage"][&class] == archive_sha256(&json_bytes(&paths)?),
            "class coverage hash mismatch",
        )?;
        for path in paths {
            require(classified.insert(path.clone()), "member classified twice")?;
            let data = member(members, &path)?;
            let source = sources.get(&path).ok_or("missing source descriptor")?;
            let entry = described.get(&path).ok_or("missing captured member")?;
            require(
                entry["class_name"] == class && &entry["source"] == source,
                "source descriptor/class mismatch",
            )?;
            let digest = archive_sha256(data);
            require(
                inventory[&path]["byteLength"].as_u64() == Some(data.len() as u64)
                    && inventory[&path]["sha256"] == digest
                    && number(source, "byte_length")? == data.len() as u64
                    && source["sha256"] == digest,
                "member hash or length mismatch",
            )?;
            require(
                string(source, "key")?.starts_with(&prefix)
                    && !string(source, "bucket")?.is_empty()
                    && !string(source, "version_id")?.is_empty()
                    && (string(source, "version_id")? != "null" || null_policy.is_some()),
                "invalid or foreign immutable source descriptor",
            )?;
        }
    }
    require(classified == actual, "unclassified member")
}

fn roots(doc: &Doc, name: &str) -> Result<BTreeMap<String, MapRef>, String> {
    let txn = doc.transact();
    let Some(map) = txn.get_map(name) else {
        return Ok(BTreeMap::new());
    };
    map.iter(&txn)
        .map(|(id, value)| {
            source_id(id)?;
            match value {
                Out::YMap(row) => Ok((id.to_string(), row)),
                _ => Err(format!("non-map {name} entry {id}")),
            }
        })
        .collect()
}
fn raw_field(doc: &Doc, row: &MapRef, key: &str) -> Value {
    match row.get(&doc.transact(), key) {
        Some(Out::Any(Any::String(s))) => json!(s.as_ref()),
        Some(Out::Any(Any::Number(n))) => json!(n),
        Some(Out::Any(Any::BigInt(n))) => json!(n),
        Some(Out::Any(Any::Bool(b))) => json!(b),
        _ => Value::Null,
    }
}
fn parsed_doc(bytes: &[u8], label: &str) -> Result<Doc, String> {
    let doc = Doc::new();
    apply_full_update(&doc, bytes, label)?;
    Ok(doc)
}
fn checked_timestamp(value: &str) -> Result<i64, String> {
    let millis = chrono::DateTime::parse_from_rfc3339(value)
        .map(|v| v.timestamp_millis())
        .map_err(|e| format!("unsupported source timestamp: {e}"))?;
    require(
        millis >= 0,
        "source timestamp predates native unsigned epoch",
    )?;
    Ok(millis)
}

fn escape_legacy_bare_xml_ampersands(xml: &str) -> String {
    let mut output = String::with_capacity(xml.len());
    let mut offset = 0;
    while offset < xml.len() {
        let tail = &xml[offset..];
        let character = tail.chars().next().expect("nonempty XML tail");
        if character != '&' {
            output.push(character);
            offset += character.len_utf8();
            continue;
        }
        let entity = &tail[1..];
        let named = ["amp;", "lt;", "gt;", "quot;", "apos;"]
            .iter()
            .any(|name| entity.starts_with(name));
        let numeric = entity.strip_prefix('#').is_some_and(|value| {
            let (digits, radix) = value
                .strip_prefix(['x', 'X'])
                .map(|digits| (digits, 16))
                .unwrap_or((value, 10));
            let Some(end) = digits.find(';') else {
                return false;
            };
            end > 0
                && digits[..end]
                    .chars()
                    .all(|digit| digit.is_digit(radix))
        });
        if named || numeric {
            output.push('&');
        } else {
            output.push_str("&amp;");
        }
        offset += 1;
    }
    output
}

#[derive(Clone, Copy)]
struct RdfOriginal<'a> {
    document_id: &'a str,
    storage_key: &'a str,
    filename: &'a str,
    mime: &'a str,
    file_type: &'a str,
    byte_length: u64,
}

pub(super) fn legacy_nonnegative_integral_lexical(value: &str) -> Option<u64> {
    let (whole, fraction) = value
        .split_once('.')
        .map(|(whole, fraction)| (whole, Some(fraction)))
        .unwrap_or((value, None));
    if whole.is_empty()
        || !whole.bytes().all(|byte| byte.is_ascii_digit())
        || fraction.is_some_and(|digits| {
            digits.is_empty() || !digits.bytes().all(|byte| byte == b'0')
        })
    {
        return None;
    }
    whole.parse().ok()
}

#[cfg(test)]
fn authored_rdf_original_matches(
    rdf: &str,
    user: &str,
    graph: &str,
    expected: &RdfOriginal<'_>,
) -> Result<bool, String> {
    authored_rdf_original_matches_with_evidence(rdf, user, graph, expected, None)
}

// Missing/null descriptive size is not missing content. This finite declaration
// is checked against the actual source field and never accepts an unequal value.
fn original_size_evidence(
    evidence: Option<&Value>, basis: &str, field: &str, presence: &str,
) -> Result<bool, String> {
    let Some(evidence) = evidence else { return Ok(false); };
    require(matches!((basis, field, presence),
        ("captured-workspace-yjs", "sizeBytes", "absent" | "null") |
        ("authored-rdf-v1", "sourceContentSize", "absent")),
        "unsupported original metadata absence")?;
    require(*evidence == json!({
        "schema":"cloud1-original-metadata-evidence.v1",
        "rule":"verified-object-size-source-absent-or-null-v1",
        "basis":basis,"field":field,"presence":presence,"sourceValue":null
    }), "original metadata evidence differs from source field")?;
    Ok(true)
}

fn original_storage_filename(filename: &str) -> Result<String, String> {
    require(!filename.is_empty() && filename.len() <= 65536,
        "original filename byte budget or empty value")?;
    // The source name remains exact provenance, never a native path. Preserve
    // already-safe names; one original per owner makes this mapping injective
    // within the storage slot, and the full digest avoids lossy sanitization.
    if filename.len() <= 200 && crate::ids::safe_filename(filename) == filename
        && !matches!(filename, "." | "..")
        && !filename.eq_ignore_ascii_case("manifest.json")
    { Ok(filename.into()) }
    else { Ok(format!("source-{}.bin", archive_sha256(filename.as_bytes()))) }
}

fn workspace_original_size_evidence(workspace: &yrs::Doc, row: &MapRef, key: &str,
    evidence: Option<&Value>) -> Result<bool, String> {
    require(matches!(key, "sizeBytes" | "sf_sizeBytes"), "unsupported original workspace size key")?;
    let presence = match row.get(&workspace.transact(), key) {
        None => "absent", Some(Out::Any(Any::Null)) => "null", _ => "present",
    };
    original_size_evidence(evidence, "captured-workspace-yjs", "sizeBytes", presence)
}

// Keep only original-file predicates, scoped to the captured owner and graph.
// Vec deliberately retains duplicate statements: multiplicity is part of the
// ownership check. Parse the entire dataset once, including its tail, so a
// malformed unrelated statement cannot be hidden by an early match.
struct OriginalRdfIndex {
    subjects: BTreeMap<String, Vec<Quad>>,
}
impl OriginalRdfIndex {
    fn parse(rdf: &str, user: &str, graph: &str) -> Result<Self, String> {
        let graph_name = format!("urn:mnemosyne:user:{user}:graph:{graph}");
        let document_prefix = format!("{graph_name}:doc:");
        let artifact_prefix = format!("{graph_name}:artifact:");
        let mut subjects: BTreeMap<String, Vec<Quad>> = BTreeMap::new();
        for quad in RdfParser::from_format(RdfFormat::NQuads).for_slice(rdf.as_bytes()) {
            let quad = quad.map_err(|error| error.to_string())?;
            if !matches!(&quad.graph_name, GraphName::NamedNode(node) if node.as_str() == graph_name) {
                continue;
            }
            let NamedOrBlankNode::NamedNode(subject) = &quad.subject else { continue; };
            let Some(predicate) = quad.predicate.as_str().strip_prefix("http://mnemosyne.dev/doc#") else { continue; };
            let document = subject.as_str().starts_with(&document_prefix) && matches!(predicate,
                "sourceStorageKey" | "sourceOriginalFilename" | "sourceMimeType" | "sourceFileType" | "sourceContentSize");
            let artifact = subject.as_str().starts_with(&artifact_prefix) && predicate == "storageKey";
            if document || artifact {
                subjects.entry(subject.as_str().into()).or_default().push(quad);
            }
        }
        Ok(Self { subjects })
    }
    fn rows(&self, subject: &str) -> &[Quad] {
        self.subjects.get(subject).map(Vec::as_slice).unwrap_or(&[])
    }
}

#[cfg(test)]
fn authored_rdf_original_matches_with_evidence(
    rdf: &str, user: &str, graph: &str, expected: &RdfOriginal<'_>,
    size_evidence: Option<&Value>,
) -> Result<bool, String> {
    let index = OriginalRdfIndex::parse(rdf, user, graph)?;
    indexed_rdf_original_matches_with_evidence(&index, user, graph, expected, size_evidence)
}

fn indexed_rdf_original_matches_with_evidence(
    index: &OriginalRdfIndex, user: &str, graph: &str, expected: &RdfOriginal<'_>,
    size_evidence: Option<&Value>,
) -> Result<bool, String> {
    const NS: &str = "http://mnemosyne.dev/doc#";
    const XSD_INTEGER: &str = "http://www.w3.org/2001/XMLSchema#integer";
    let subject = format!("urn:mnemosyne:user:{user}:graph:{graph}:doc:{}", expected.document_id);
    let mut values: BTreeMap<String, Vec<(String, String)>> = BTreeMap::new();
    for quad in index.rows(&subject) {
        let predicate = quad.predicate.as_str().strip_prefix(NS).expect("indexed predicate namespace");
        let Term::Literal(literal) = &quad.object else {
            return Ok(false);
        };
        values.entry(predicate.into()).or_default().push((
            literal.value().into(),
            literal.datatype().as_str().into(),
        ));
    }
    let one = |predicate: &str| -> Option<&(String, String)> {
        let rows = values.get(predicate)?;
        (rows.len() == 1).then(|| &rows[0])
    };
    let size = one("sourceContentSize");
    let absent_size = original_size_evidence(size_evidence, "authored-rdf-v1",
        "sourceContentSize", if values.contains_key("sourceContentSize") { "present" } else { "absent" })?;
    let plain = |predicate: &str, expected: &str| one(predicate).is_some_and(|value|value.0==expected && value.1=="http://www.w3.org/2001/XMLSchema#string");
    Ok(plain("sourceStorageKey",expected.storage_key)
        && plain("sourceOriginalFilename",expected.filename)
        && plain("sourceMimeType",expected.mime)
        && plain("sourceFileType",expected.file_type)
        && (absent_size || size.is_some_and(|value| {
            value.1 == XSD_INTEGER
                && legacy_nonnegative_integral_lexical(&value.0) == Some(expected.byte_length)
        })))
}

fn prepare(bytes: &[u8], operation: &CrdtOperation) -> Result<Prepared, String> {
    let members = unpack(bytes)?;
    let manifest = parse_json(member(&members, "manifest.json")?)?;
    validate_envelope(&members, &manifest, operation, bytes)?;
    exact_class(&manifest, "workspace", set(&["crdt/workspace.yjs"]))?;
    exact_class(&manifest, "rdf", if members.contains_key(DATASET_MEMBER) {
        set(&["rdf/dataset.nq", DATASET_MEMBER])
    } else { set(&["rdf/dataset.nq"]) })?;
    let graph = operation.graph_id.as_str();
    let user = string(&manifest["source"], "userId")?;
    let prefix = format!("users/{user}/graphs/{graph}/");
    let source_objects = object(&manifest, "sourceObjects")?;
    let workspace = parsed_doc(member(&members, "crdt/workspace.yjs")?, "source workspace")?;
    let documents = roots(&workspace, "documents")?;
    let artifacts = roots(&workspace, "artifacts")?;
    let custody_index = members.get("source-custody/index.json")
        .map(|bytes| parse_json(bytes)).transpose()?;
    let mut unavailable_ids = BTreeSet::new();
    let mut unavailable_bodies = Vec::new();
    if let Some(rows) = custody_index.as_ref().and_then(|c| c.get("unavailableBodies")) {
        require(manifest["transformation"] == V23, "unavailable bodies require v2.3")?;
        let custody = custody_index.as_ref().unwrap();
        for row in rows.as_array().ok_or("unavailable bodies must be an array")? {
            let id = string(row, "documentId")?;
            source_id(id)?;
            require(row.as_object().map(|o| o.len()) == Some(5)
                && row["reason"] == "absent-current-saved-object"
                && row["sourceUserId"] == user && row["sourceGraphId"] == graph
                && row["inventorySha256"] == custody["inventorySha256"]
                && documents.contains_key(id) && unavailable_ids.insert(id.to_string()),
                "unavailable body source/identity mismatch")?;
            require_lower_sha256(string(row, "inventorySha256")?, "unavailable body inventory")?;
            let key = format!("{prefix}documents/{id}.yjs");
            require(!array(custody, "objects")?.iter().any(|o|
                o["latest"] == true && o["key"] == key),
                "unavailable body has current source bytes")?;
            let mut retained = row.clone();
            retained["metadata"] = serde_json::to_value(yrs::types::ToJson::to_json(&documents[id], &workspace.transact()))
                .map_err(|e| e.to_string())?;
            unavailable_bodies.push(retained);
        }
    }
    // A malformed entity must not disappear through a materializer's filter.
    let _folders = roots(&workspace, "folders")?;
    let _wires = roots(&workspace, "wires")?;
    let document_paths: BTreeSet<_> = documents
        .keys()
        .filter(|id| !unavailable_ids.contains(*id))
        .map(|id| format!("crdt/documents/{id}.yjs"))
        .collect();
    exact_class(&manifest, "documents", document_paths)?;
    let mut live = Vec::new();
    for id in documents.keys() {
        if unavailable_ids.contains(id) { continue; }
        let body = member(&members, &format!("crdt/documents/{id}.yjs"))?;
        let _ = parsed_doc(body, "live document")?;
        live.push((id.clone(), body.to_vec()));
    }
    let mut expected_boundaries = set(&["workspace"]);
    expected_boundaries.extend(documents.keys().filter(|id| !unavailable_ids.contains(*id)).map(|id| format!("document:{id}")));
    let mut boundaries = BTreeSet::new();
    if manifest["transformation"] == V23 {
        for boundary in array(&manifest["source"]["capture"], "snapshotBoundaries")? {
            let entity = string(boundary, "entity")?;
            let path = if entity == "workspace" {
                "crdt/workspace.yjs".to_string()
            } else {
                let id = entity
                    .strip_prefix("document:")
                    .ok_or("unknown snapshot entity")?;
                require(
                    documents.contains_key(id),
                    "snapshot boundary names absent document",
                )?;
                format!("crdt/documents/{id}.yjs")
            };
            require(
                boundaries.insert(entity.to_string())
                    && boundary["member"] == path
                    && Some(&boundary["source"]) == source_objects.get(&path),
                "duplicate or wrong source snapshot boundary",
            )?;
        }
    } else {
        for boundary in array(&manifest["source"]["capture"], "journalBoundaries")? {
            require(
                boundaries.insert(string(boundary, "entity")?.to_string()),
                "duplicate journal entity",
            )?;
            number(boundary, "epoch")?;
            number(boundary, "sequence")?;
            require_lower_sha256(string(boundary, "event_hash")?, "journal event hash")?;
        }
    }
    require(
        boundaries == expected_boundaries,
        "source boundaries do not cover live CRDT entities",
    )?;
    let old_manifest = GraphArchiveManifest {
        source_user_id: user.to_string(),
        source_graph_id: graph.to_string(),
        source_graph_title: None,
        source_graph_description: None,
        includes_artifacts: true,
    };
    let source_rdf =
        std::str::from_utf8(member(&members, "rdf/dataset.nq")?).map_err(|e| e.to_string())?;
    let original_rdf = OriginalRdfIndex::parse(source_rdf, user, graph)?;
    let mut original_paths = set(&["originals/index.json"]);
    let mut original_owners = BTreeSet::new();
    let mut original_keys = BTreeSet::new();
    let original_index = parse_json(member(&members, "originals/index.json")?)?;
    let mut originals = Vec::new();
    let mut original_assertions = HashMap::new();
    for entry in original_index
        .as_array()
        .ok_or("original index must be an array")?
    {
        let kind = string(entry, "ownerKind")?;
        let id = string(entry, "id")?;
        source_id(id)?;
        let path = string(entry, "member")?;
        let filename = string(entry, "filename")?;
        let storage_filename = original_storage_filename(filename)?;
        require(
            path.starts_with("originals/")
                && path != "originals/index.json"
                && original_paths.insert(path.to_string())
                && original_owners.insert((kind.to_string(), id.to_string())),
            "duplicate or noncanonical original owner/path",
        )?;
        require(
            entry["textOwnership"] == "absent",
            "source HTML ownership is not destination authority",
        )?;
        let body = member(&members, path)?;
        require(
            number(entry, "byteLength")? == body.len() as u64
                && entry["sha256"] == archive_sha256(body),
            "original payload hash or size mismatch",
        )?;
        let source = source_objects
            .get(path)
            .ok_or("original source descriptor missing")?;
        let key = string(entry, "sourceStorageKey")?;
        require(
            source["key"] == key && key.starts_with(&prefix),
            "original source key mismatch",
        )?;
        original_keys.insert(key.to_string());
        let mime = original_effective_mime(entry, &manifest)?;
        let size_evidence = entry.get("sourceMetadataEvidence");
        match kind {
            "document" | "artifact" => {
                let rows = if kind == "document" {
                    &documents
                } else {
                    &artifacts
                };
                let row = rows.get(id).ok_or("original owner absent from workspace")?;
                let keys = if kind == "document" {
                    [
                        "sf_storageKey",
                        "sf_originalFilename",
                        "sf_mimeType",
                        "sf_sizeBytes",
                    ]
                } else {
                    ["storageKey", "originalFilename", "mimeType", "sizeBytes"]
                };
                let workspace_absent_size = if entry.get("ownershipBasis").is_none() {
                    workspace_original_size_evidence(&workspace, row, keys[3], size_evidence)?
                } else { false };
                let workspace_match = raw_field(&workspace, row, keys[0]) == key
                    && raw_field(&workspace, row, keys[1]) == filename
                    && raw_field(&workspace, row, keys[2]) == mime
                    && (workspace_absent_size || raw_field(&workspace, row, keys[3]).as_f64() == Some(body.len() as f64));
                let ownership_basis = entry.get("ownershipBasis").and_then(Value::as_str);
                let rdf_match = if ownership_basis == Some("authored-rdf-v1") && kind == "document"
                {
                    require(
                        manifest["transformation"] == V23
                            && keys.iter().chain(["sf_fileType","sourceFile"].iter()).all(|field|row.get(&workspace.transact(),*field).is_none()),
                        "RDF original ownership conflicts with workspace metadata",
                    )?;
                    indexed_rdf_original_matches_with_evidence(
                        &original_rdf,
                        user,
                        graph,
                        &RdfOriginal {
                            document_id: id,
                            storage_key: key,
                            filename,
                            mime,
                            file_type: string(entry, "fileType")?,
                            byte_length: body.len() as u64,
                        },
                        size_evidence,
                    )?
                } else {
                    require(
                        ownership_basis.is_none(),
                        "unsupported original ownership basis",
                    )?;
                    false
                };
                require(
                    workspace_match || rdf_match,
                    "workspace/RDF original metadata join mismatch",
                )?;
                if workspace_match && kind == "artifact" && manifest["transformation"] == V23 {
                    let source_graph=format!("urn:mnemosyne:user:{user}:graph:{graph}");
                    let source_subject=format!("{source_graph}:artifact:{id}");
                    let subject=format!("urn:mnemosyne:local:graph:{graph}:artifact:{id}");
                    let native_key=key.replace(&format!("users/{user}/"),"users/default/");
                    for quad in original_rdf.rows(&source_subject) {
                        if !matches!(&quad.subject,NamedOrBlankNode::NamedNode(s) if s.as_str()==source_subject)
                            || !matches!(&quad.graph_name,GraphName::NamedNode(g) if g.as_str()==source_graph)
                            || quad.predicate.as_str()!="http://mnemosyne.dev/doc#storageKey" { continue; }
                        require(quad.object == Term::Literal(oxigraph::model::Literal::new_simple_literal(key)),
                            "artifact RDF storage key differs from captured owned original")?;
                        if native_key==key {continue;}
                        let statement=format!("<{subject}> {} {}",quad.predicate,quad.object);
                        require(original_assertions.insert(statement,json!({"subject":subject,"predicate":quad.predicate.as_str(),
                            "rule":"owned-artifact-storage-key-rewrite-v1","authority":"validated-original-index-and-captured-workspace",
                            "source":{"lexical":key,"datatype":"http://www.w3.org/2001/XMLSchema#string"},
                            "native":{"lexical":native_key,"datatype":"http://www.w3.org/2001/XMLSchema#string"},
                            "authoritativeProjectionField":{"root":"artifacts","entityId":id,"key":"storageKey","presence":"present","value":key},
                            "sourceUserId":user,"sourceGraphId":graph,"rawSourceRetained":true,"fetchAuthority":false,
                            "original":{"ownerKind":kind,"id":id,"sourceMember":path,"sha256":archive_sha256(body),"byteLength":body.len(),
                                "indexMember":"originals/index.json","indexSha256":archive_sha256(member(&members,"originals/index.json")?),
                                "nativePath":format!("artifacts/{id}/original/{storage_filename}")}})).is_none(),"duplicate artifact storage-key assertion")?;
                    }
                }
                if rdf_match {
                    let source_subject=format!("urn:mnemosyne:user:{user}:graph:{graph}:doc:{id}");
                    let source_graph=format!("urn:mnemosyne:user:{user}:graph:{graph}");
                    for quad in original_rdf.rows(&source_subject) {
                        if !matches!(&quad.subject,NamedOrBlankNode::NamedNode(s) if s.as_str()==source_subject)
                            || !matches!(&quad.graph_name,GraphName::NamedNode(g) if g.as_str()==source_graph) { continue; }
                        let Some(name)=quad.predicate.as_str().strip_prefix("http://mnemosyne.dev/doc#") else { continue; };
                        let field=match name {"sourceStorageKey"=>"sf_storageKey","sourceOriginalFilename"=>"sf_originalFilename",
                            "sourceMimeType"=>"sf_mimeType","sourceFileType"=>"sf_fileType","sourceContentSize"=>"sf_sizeBytes",_=>continue};
                        let Term::Literal(value)=&quad.object else { return Err("original metadata must remain literal".into()); };
                        let subject=crate::rdf::document_subject(id);
                        let statement=format!("<{}> {} {}",subject,quad.predicate,quad.object);
                        require(original_assertions.insert(statement,json!({"subject":subject,"predicate":quad.predicate.as_str(),
                            "source":{"lexical":value.value(),"datatype":value.datatype().as_str()},"native":null,
                            "authoritativeProjectionField":{"root":"documents","entityId":id,"key":field,"presence":"absent"},
                            "authority":"validated-original-index-and-source-rdf","disposition":"retained-not-rematerialized",
                            "reason":"owned-original-provenance-not-current-workspace-metadata","rawSourceRetained":true,
                            "original":{"ownerKind":kind,"id":id,"sourceMember":path,"sha256":archive_sha256(body),"byteLength":body.len(),
                                "indexMember":"originals/index.json","indexSha256":archive_sha256(member(&members,"originals/index.json")?),
                                "nativePath":format!("documents/{id}/original/{storage_filename}")}})).is_none(),"duplicate original RDF disposition")?;
                    }
                }
            }
            "image" => require(
                size_evidence.is_none() && key.starts_with(&format!("{prefix}images/")),
                "foreign image key",
            )?,
            _ => return Err("unsupported original owner kind".into()),
        }
        originals.push(Original {
            kind: kind.into(),
            id: id.into(),
            filename: storage_filename.clone(),
            mime: mime.into(),
            member: path.into(),
            provenance: json!({"ownerKind":kind,"id":id,"ownershipBasis":entry.get("ownershipBasis").cloned().unwrap_or(json!("captured-workspace-yjs")),
                "sourceMember":path,"sha256":archive_sha256(body),"byteLength":body.len(),
                "indexMember":"originals/index.json","indexSha256":archive_sha256(member(&members,"originals/index.json")?),
                "sourceMimeType":entry["mimeType"],"effectiveMimeType":mime,
                "sourceFilename":filename,"storageFilename":storage_filename,
                "filenameDisposition":if filename==storage_filename {"source-asserted-safe-name"} else {"opaque-storage-key-exact-source-filename-retained-v1"},
                "sourceMetadataEvidence":size_evidence,
                "mimeDisposition":if entry["mimeType"].is_null() {"native-opaque-default-source-mime-null"} else {"source-asserted"},
                "legacyRdfMetadataPromoted":false,"nativeMetadataTimestamps":"installation-time"}),
        });
    }
    exact_class(&manifest, "originals", original_paths)?;
    for (rows, field) in [(&documents, "sf_storageKey"), (&artifacts, "storageKey")] {
        for row in rows.values() {
            if let Some(key) = raw_field(&workspace, row, field)
                .as_str()
                .filter(|v| !v.is_empty())
            {
                require(
                    original_keys.contains(key),
                    "referenced original bytes missing",
                )?;
            }
        }
    }
    let (mut history, mut legacy_history) = prepare_history_partition(&members, &manifest, graph, user, true)?;
    let (unavailable_history, history_availability) = prepare_history_availability(&members, &manifest, graph, user)?;
    let graph_history = prepare_graph_history(&members, &manifest, &old_manifest, graph)?;
    let deletion_index = parse_json(member(&members, "deletions/index.json")?)?;
    let mut deletion_paths = set(&["deletions/index.json"]);
    let mut deleted_ids = BTreeSet::new();
    for entry in deletion_index
        .as_array()
        .ok_or("deletion index must be an array")?
    {
        let id = string(entry, "documentId")?;
        source_id(id)?;
        let path = string(entry, "member")?;
        require(
            !documents.contains_key(id) && deleted_ids.insert(id.to_string()),
            "duplicate or live/deleted identity conflict",
        )?;
        require(
            entry["sourceKind"] == "cloud1-s3-zero-byte-marker"
                && entry.get("deletionId") == Some(&Value::Null)
                && entry.get("deletedAt") == Some(&Value::Null),
            "unsupported or invented source deletion authority",
        )?;
        require(
            path == format!("deletions/{id}.tombstone")
                && deletion_paths.insert(path.to_string())
                && member(&members, path)?.is_empty(),
            "invalid deletion marker",
        )?;
        require(
            entry["sourceKey"] == format!("{prefix}documents/{id}.tombstone")
                && source_objects.get(path).ok_or("missing deletion source")?["key"]
                    == entry["sourceKey"],
            "deletion source reference mismatch",
        )?;
    }
    exact_class(&manifest, "deletions", deletion_paths)?;
    // Histories may legitimately outlive live documents. Keep those histories
    // and apply the source deletion barrier; never resurrect from a history row.
    (history,legacy_history)=history_owner_partition(&members,&manifest,history,legacy_history,
        &documents.keys().cloned().collect(),&deleted_ids)?;
    let history_disposition = history_disposition(&members,&manifest,&history,&legacy_history,&unavailable_history)?;
    let metadata = prepare_custody(&members, &manifest, graph, user)?;
    let dataset_partitions = prepare_dataset_partitions(&members, &manifest, graph, user, &archive_sha256(bytes))?;
    let mut parsed = ParsedGraphArchive {
        manifest: old_manifest,
        rdf_n_quads: String::new(),
        workspace_bytes: Some(member(&members, "crdt/workspace.yjs")?.to_vec()),
        documents: live,
        warnings: Vec::new(),
    };
    // The admission standard is a property of THIS import, declared in its own plan and
    // covered by the envelope hash, not an ambient mode. Absent means strict.
    let concessions = crate::crdt_engine::content_parity::Concessions::parse(
        operation.payload.get("contentParity"))?;
    let (rdf, graph_mapping, regenerated, rdf_count, named_count, timestamp_normalization, derived_disposition) =
        prepare_rdf_with_content_parity_and_partitions(source_rdf, &manifest, &mut parsed, graph, &original_assertions,
            &unavailable_ids, Some(&archive_sha256(bytes)), &concessions, Some(&dataset_partitions))?;
    let normalized=timestamp_normalization["entries"].as_array().map_or(0,Vec::len);
    // Conceded material is retained in its own evidence graph and reported on its own
    // schema, so the source-only wire disposition keeps describing exactly the wire
    // anatomy and its quad-for-quad coverage contract stays literally true. The
    // ARITHMETIC below still counts both, because both are retained rather than
    // authored and the exhaustiveness check must account for every source quad.
    let mut derived_disposition = derived_disposition;
    let all_entries = derived_disposition["entries"].as_array().cloned().unwrap_or_default();
    let (parity_entries, native_entries): (Vec<Value>, Vec<Value>) = all_entries
        .iter().cloned().partition(|row| row.get("contentParity").is_some());
    derived_disposition["entries"] = json!(native_entries);
    let content_parity_disposition = json!({"schema":"cloud1-content-parity-disposition.v1",
        "ruling":crate::crdt_engine::content_parity::RULING,
        "concessions":concessions.declared(),"entries":parity_entries});
    let retained=all_entries.len();
    let evidence=all_entries.iter().filter(|row|row["evidenceGraph"].is_string()).count();
    let parity_retained=content_parity_disposition["entries"].as_array().map_or(0,Vec::len);
    let authored=count_archive_nquads(&rdf)?.checked_sub(evidence).ok_or("source evidence exceeds stored RDF")?;
    require(normalized<=regenerated && authored.checked_add(regenerated).and_then(|n|n.checked_add(retained))==Some(rdf_count),"RDF preservation accounting is not exhaustive")?;
    let rdf_accounting=json!({"schema":"cloud1-rdf-preservation-accounting.v1","sourceQuadCount":rdf_count,
        "exactMappedSourceCount":rdf_count-normalized-retained,"normalizedSourceCount":normalized,
        "legacyEvidenceOnlySourceCount":retained,"nativeAuthoredQuadCount":authored,
        "nativeEvidenceQuadCount":evidence-parity_retained,"contentParityQuadCount":parity_retained,
        "exactRegeneratedSourceCount":regenerated-normalized,"regeneratedSourceCount":regenerated,
        "complete":true,"rdfMappedTestimonyFullSet":normalized==0 && retained==0});
    let counts = json!({"documents":documents.len(),"originals":originals.len(),"rdfQuads":rdf_count,
        "namedGraphs":named_count,"documentSnapshots":history.len()+legacy_history.len(),"graphSnapshots":graph_history.len(),"tombstones":deleted_ids.len()});
    require(
        manifest["counts"] == counts,
        "manifest semantic counts mismatch",
    )?;
    require(
        operation.payload["expectedDocumentCount"] == counts["documents"]
            && operation.payload["expectedRdfTripleCount"] == counts["rdfQuads"],
        "plan semantic count mismatch",
    )?;
    rewrite_graph_archive_workspace(&workspace, &parsed.manifest, graph);
    let snapshot =
        super::super::workspace_ops::materialize_workspace_snapshot_json(graph, &workspace)?;
    Ok(Prepared {
        members,
        manifest,
        parsed,
        workspace: encode_full_state(&workspace),
        snapshot,
        originals,
        history,
        graph_history,
        deletions: deleted_ids.into_iter().collect(),
        rdf,
        graph_mapping,
        dataset_partitions,
        timestamp_normalization,
        derived_disposition,
        content_parity_disposition,
        rdf_accounting,
        metadata,
        regenerated,
        unavailable_bodies,
        unavailable_history,
        history_availability,
        history_disposition,
    })
}

fn prepare_rdf(
    rdf: &str,
    manifest: &Value,
    parsed: &mut ParsedGraphArchive,
    graph: &str,
) -> Result<(String, Value, usize, usize, usize, Value, Value), String> {
    prepare_rdf_with_originals(rdf,manifest,parsed,graph,&HashMap::new())
}

fn prepare_rdf_with_originals(rdf: &str, manifest: &Value, parsed: &mut ParsedGraphArchive, graph: &str, original_assertions: &HashMap<String,Value>) -> Result<(String, Value, usize, usize, usize, Value, Value), String> {
    prepare_rdf_with_availability(rdf, manifest, parsed, graph, original_assertions, &BTreeSet::new())
}

fn prepare_rdf_with_availability(rdf: &str, manifest: &Value, parsed: &mut ParsedGraphArchive, graph: &str, original_assertions: &HashMap<String,Value>, unavailable_ids: &BTreeSet<String>) -> Result<(String, Value, usize, usize, usize, Value, Value), String> {
    prepare_rdf_with_evidence_context(rdf,manifest,parsed,graph,original_assertions,unavailable_ids,None)
}

fn prepare_rdf_with_evidence_context(rdf: &str, manifest: &Value, parsed: &mut ParsedGraphArchive, graph: &str, original_assertions: &HashMap<String,Value>, unavailable_ids: &BTreeSet<String>, evidence_archive_sha256: Option<&str>) -> Result<(String, Value, usize, usize, usize, Value, Value), String> {
    prepare_rdf_with_content_parity(rdf, manifest, parsed, graph, original_assertions, unavailable_ids,
        evidence_archive_sha256, &crate::crdt_engine::content_parity::Concessions::none())
}

fn prepare_rdf_with_content_parity(rdf: &str, manifest: &Value, parsed: &mut ParsedGraphArchive, graph: &str, original_assertions: &HashMap<String,Value>, unavailable_ids: &BTreeSet<String>, evidence_archive_sha256: Option<&str>, concessions: &crate::crdt_engine::content_parity::Concessions) -> Result<(String, Value, usize, usize, usize, Value, Value), String> {
    prepare_rdf_with_content_parity_and_partitions(rdf, manifest, parsed, graph, original_assertions,
        unavailable_ids, evidence_archive_sha256, concessions, None)
}

fn prepare_rdf_with_content_parity_and_partitions(rdf: &str, manifest: &Value, parsed: &mut ParsedGraphArchive, graph: &str, original_assertions: &HashMap<String,Value>, unavailable_ids: &BTreeSet<String>, evidence_archive_sha256: Option<&str>, concessions: &crate::crdt_engine::content_parity::Concessions, dataset_partitions: Option<&Value>) -> Result<(String, Value, usize, usize, usize, Value, Value), String> {
    let source = format!(
        "urn:mnemosyne:user:{}:graph:{graph}",
        parsed.manifest.source_user_id
    );
    let target = crate::rdf::graph_subject(graph);
    let retained = dataset_partitions.filter(|v| !v.is_null());
    let mut mapping = BTreeMap::new();
    for entry in array(manifest, "namedGraphs")? {
        let name = string(entry, "iri")?;
        let disposition = string(entry, "disposition")?;
        let destination = if name == source {
            require(
                disposition == "core-v3-authority-partition",
                "main RDF disposition mismatch",
            )?;
            crate::rdf_authority::user_rdf_graph_iri(graph)
        } else if let Some(partitions) = retained {
            require(disposition == "retained-source-testimony", "retained RDF disposition mismatch")?;
            array(partitions, "partitions")?.iter().find(|row| row["identity"] == json!({"kind":"named","iri":name}))
                .and_then(|row| row["destinationGraph"].as_str()).ok_or("retained named partition missing")?.to_string()
        } else {
            require(
                name.starts_with(&format!("{source}:"))
                    && !name.contains(":projection:")
                    && disposition == "preserve-separate-authored-graph",
                "foreign/derived/unknown source RDF graph",
            )?;
            format!(
                "{target}:user:preserved:{}",
                archive_sha256(name.as_bytes())
            )
        };
        require(
            mapping.insert(name.to_string(), destination).is_none(),
            "duplicate named RDF graph",
        )?;
    }
    // An actually empty dataset has no named graph to list. Owner/graph and
    // catalogue custody were already validated by prepare_custody; no RDF
    // statement is synthesized to stand in for that independent identity.
    if mapping.is_empty() {
        require(
            manifest["counts"]["rdfQuads"] == json!(0)
                && manifest["counts"]["namedGraphs"] == json!(0),
            "empty RDF inventory count mismatch",
        )?;
    } else {
        require(mapping.contains_key(&source), "main RDF inventory missing")?;
    }
    let ids: BTreeSet<_> = parsed.documents.iter().map(|(id, _)| id.as_str())
        .chain(unavailable_ids.iter().map(String::as_str)).collect();
    let rewrite = |node: NamedNode| -> Result<NamedNode, String> {
        if let Some(tail) = node.as_str().strip_prefix(&format!("{source}:doc:")) {
            let (id, fragment) = tail
                .split_once('#')
                .map(|(id, tail)| (id, format!("#{tail}")))
                .unwrap_or((tail, String::new()));
            require(ids.contains(id), "unresolved source document RDF identity")?;
            return NamedNode::new(format!("{}{fragment}", crate::rdf::document_subject(id)))
                .map_err(|e| e.to_string());
        }
        if let Some(suffix) = node.as_str().strip_prefix(&source) {
            if suffix.is_empty() || suffix.starts_with([':', '/', '#']) {
                return NamedNode::new(format!("{target}{suffix}")).map_err(|e| e.to_string());
            }
        }
        Ok(node)
    };
    let mut seen = BTreeSet::new();
    let mut quad_set = BTreeSet::new();
    let mut side = String::new();
    let mut main = String::new();
    for quad in RdfParser::from_format(RdfFormat::NQuads).for_slice(rdf.as_bytes()) {
        let mut quad = quad.map_err(|e| e.to_string())?;
        require(
            quad_set.insert(quad.to_string()),
            "duplicate RDF statement testimony",
        )?;
        if quad.graph_name == GraphName::DefaultGraph {
            let partitions = retained.ok_or("unclassified default RDF graph")?;
            let destination = array(partitions, "partitions")?.iter().find(|row| row["identity"] == json!({"kind":"default"}))
                .and_then(|row| row["destinationGraph"].as_str()).ok_or("retained default partition missing")?;
            quad.graph_name = NamedNode::new(destination).map_err(|e|e.to_string())?.into();
            side.push_str(&format!("{quad} .\n"));
            continue;
        }
        let GraphName::NamedNode(name) = &quad.graph_name else { return Err("blank RDF graph unsupported".into()); };
        let destination = mapping
            .get(name.as_str())
            .ok_or("unclassified named RDF graph")?;
        seen.insert(name.as_str().to_string());
        if name.as_str() == source {
            main.push_str(&format!("{quad} .\n"));
            continue;
        }
        if retained.is_none() {
        if let NamedOrBlankNode::NamedNode(node) = quad.subject {
            quad.subject = rewrite(node)?.into();
        }
        quad.predicate = rewrite(quad.predicate)?;
        if let Term::NamedNode(node) = quad.object {
            quad.object = rewrite(node)?.into();
        }
        }
        quad.graph_name = NamedNode::new(destination)
            .map_err(|e| e.to_string())?
            .into();
        side.push_str(&format!("{quad} .\n"));
    }
    require(
        seen == mapping.keys().cloned().collect(),
        "empty or missing named graph testimony",
    )?;
    parsed.rdf_n_quads = main;
    let preflight = preflight_cell_archive_with_content_parity(parsed, graph, manifest["transformation"] == V23, original_assertions, unavailable_ids, evidence_archive_sha256, concessions)?;
    let output = format!("{}{side}", preflight.user_rdf);
    crate::rdf_query_service::validate_rdf_dataset_graph_targets(
        graph,
        &output,
        "application/n-quads",
        None,
    )?;
    Ok((
        output,
        json!(mapping),
        preflight.regenerated_statements,
        quad_set.len(),
        mapping.len(),
        json!({"schema":"cloud1-native-projection-normalization.v1", "sourceTransformation":manifest["transformation"],
            "entries":preflight.legacy_timestamp_normalizations}),
        json!({"schema":"cloud1-workspace-derived-assertion-disposition.v1", "sourceTransformation":manifest["transformation"],
            "sourceUserId":manifest["source"]["userId"],"sourceGraphId":manifest["source"]["graphId"],
            "entries":preflight.retained_derived_assertions}),
    ))
}

// Named interpretation of the legacy export, NOT recovery of historical Yjs:
// str(XmlText) does not distinguish literal mark-looking text from real marks.
// Recognized balanced mark tags are interpreted as marks; other angle syntax
// and all text ampersands are literal. Original export bytes remain in custody.
fn encode_legacy_history_text(text: &str, code: bool) -> Result<String, String> {
    let mut result = String::with_capacity(text.len());
    let mut offset = 0;
    while offset < text.len() {
        let tail = &text[offset..];
        if !code && tail.starts_with('<') {
            if let Some(end) = tail.find('>') {
                let token = &tail[..=end];
                let name = token.trim_start_matches('<').trim_start_matches('/')
                    .split(|c: char| c.is_whitespace() || c == '>').next().unwrap_or("");
                if matches!(name,"strong"|"bold"|"em"|"italic"|"s"|"strike"|"code"|"a"|"link") {
                    // XML parsing below checks balanced tags; the mark validator
                    // checks the finite attribute vocabulary. No HTML fallback.
                    result.push_str(&token.replace('&',"&amp;"));
                    offset += token.len();
                    continue;
                }
            }
        }
        let ch = tail.chars().next().ok_or("invalid legacy text offset")?;
        match ch { '<' => result.push_str("&lt;"), '>' => result.push_str("&gt;"), '&' => result.push_str("&amp;"), _ => result.push(ch) }
        offset += ch.len_utf8();
    }
    Ok(result)
}

// The legacy blocks emitter concatenates XmlText and skips wikilinks. The
// observed tagChip/footnote nodes are empty elements too. Admit those structural
// frames only when removing them leaves the EXACT raw source text; atom-looking
// text which is present in that source text stays literal via the earlier path.
fn legacy_inline_atom_frame(raw: &str, text: &str) -> Result<Option<String>, String> {
    let mut cursor = 0;
    let mut raw_text = String::new();
    let mut encoded = String::new();
    let mut atoms = 0;
    while let Some((start, kind)) = ["wikilink", "tagChip", "footnote"].into_iter()
        .filter_map(|kind| raw[cursor..].find(&format!("<{kind} ")).map(|n|(cursor+n,kind)))
        .min_by_key(|(start,_)|*start) {
        let close = format!("</{kind}>");
        let Some(end) = raw[start..].find(&close).map(|n|start+n+close.len()) else { return Ok(None); };
        let atom = &raw[start..end];
        let parsed = roxmltree::Document::parse(atom).map_err(|_|"legacy inline atom XML invalid")?;
        let node = parsed.root_element();
        require(node.tag_name().name() == kind && node.children().next().is_none()
            && node.attribute("data-block-id").is_none(),"legacy inline atom is not an exact leaf")?;
        let allowed: &[&str] = match kind {
            "wikilink" => &["label","blockPreview","targetDocId","targetGraphId","targetBlockId","wireId"],
            "tagChip" => &["name","date"],
            _ => &["content"],
        };
        require(node.attributes().all(|attr|allowed.contains(&attr.name())),"unsupported legacy inline atom attribute")?;
        let gap = &raw[cursor..start];
        raw_text.push_str(gap);
        encoded.push_str(&encode_legacy_history_text(gap,false)?);
        encoded.push_str(atom);
        cursor=end; atoms+=1;
    }
    raw_text.push_str(&raw[cursor..]);
    if atoms == 0 || raw_text != text { return Ok(None); }
    encoded.push_str(&encode_legacy_history_text(&raw[cursor..],false)?);
    Ok(Some(encoded))
}

fn legacy_history_export_xml(xml: &str, source: &[Value]) -> Result<String, String> {
    // Legacy text is not necessarily valid XML yet. Retain the exact lexical
    // opening-tag scan, but perform it once per supported tag instead of once
    // per block. Keep every candidate so ambiguity/overlap still refuses.
    let mut openings: BTreeMap<(String, String), Vec<usize>> = BTreeMap::new();
    for kind in ["paragraph", "heading", "codeBlock", "blockquote"] {
        let prefix = format!("<{kind}");
        for (start,_) in xml.match_indices(&prefix) {
            let tail = &xml[start + prefix.len()..];
            if !tail.starts_with(|c: char| c.is_whitespace() || c == '>') { continue; }
            let mut quote = None;
            let mut open_end = None;
            for (index,ch) in xml[start..].char_indices() {
                match (quote,ch) {
                    (None, '\''|'"') => quote = Some(ch),
                    (Some(q),c) if q == c => quote = None,
                    (None,'>') => { open_end = Some(start+index+1); break; },
                    _ => (),
                }
            }
            let Some(begin) = open_end else { continue; };
            let open = &xml[start..begin];
            let empty = format!("{}/>", &open[..open.len()-1]);
            let Ok(tag) = roxmltree::Document::parse(&empty) else { continue; };
            let Some(id) = tag.root_element().attribute("data-block-id") else { continue; };
            openings.entry((kind.to_string(), id.to_string())).or_default().push(begin);
        }
    }
    let mut replacements = Vec::new();
    for row in source {
        let kind = string(row,"type")?;
        if kind == "horizontalRule" { continue; }
        require(matches!(kind,"paragraph"|"heading"|"codeBlock"|"blockquote"), "unsupported legacy text block")?;
        let id = string(row,"id")?;
        let text = string(row,"text")?;
        let closing = format!("</{kind}>");
        let mut frames = Vec::new();
        for &begin in openings.get(&(kind.to_string(), id.to_string())).map(Vec::as_slice).unwrap_or(&[]) {
            let Some(end) = begin.checked_add(text.len()) else { continue; };
            if xml.get(begin..end) == Some(text) && xml.get(end..).is_some_and(|tail| tail.starts_with(&closing)) {
                frames.push((begin,end,encode_legacy_history_text(text,kind == "codeBlock")?));
            } else if matches!(kind,"paragraph"|"heading") {
                if let Some(end) = xml[begin..].find(&closing).map(|n|begin+n) {
                    if let Some(encoded) = legacy_inline_atom_frame(&xml[begin..end],text)? {
                        frames.push((begin,end,encoded));
                    }
                }
            }
        }
        if frames.is_empty() && kind == "blockquote" {
            // A legacy blockquote may contain a real paragraph element, while
            // blocks_snapshot.text concatenates its XmlText descendants. Do
            // not replace that structure with the concatenated text. Require a
            // valid unique source subtree; the tree/content validator below
            // checks every retained structural ID and its native interpretation.
            let escaped = escape_legacy_bare_xml_ampersands(xml);
            let wrapped = format!("<doc>{escaped}</doc>");
            let tree = roxmltree::Document::parse(&wrapped).map_err(|_| "legacy composite history XML invalid")?;
            require(tree.descendants().filter(|n| n.is_element() && n.attribute("data-block-id") == Some(id)
                && n.tag_name().name() == kind).count() == 1,"legacy composite history identity ambiguous")?;
            continue;
        }
        require(frames.len() == 1,"legacy history text frame absent or ambiguous")?;
        replacements.push(frames.remove(0));
    }
    replacements.sort_by_key(|r| r.0);
    for pair in replacements.windows(2) {
        require(pair[0].1 <= pair[1].0,"overlapping legacy history text frames")?;
    }
    let mut output = String::new();
    let mut cursor = 0;
    for (begin,end,text) in replacements {
        output.push_str(&xml[cursor..begin]); output.push_str(&text); cursor = end;
    }
    output.push_str(&xml[cursor..]);
    Ok(escape_legacy_bare_xml_ampersands(&output))
}

fn legacy_history_marked_text_matches(
    text: &str,
    block: &crate::document_types::BlockSnapshot,
    native: Option<&Value>,
) -> Result<bool, String> {
    let escaped = encode_legacy_history_text(text, false)?;
    let wrapped = format!("<paragraph data-block-id=\"legacy-history-text\">{escaped}</paragraph>");
    let tree = roxmltree::Document::parse(&wrapped).map_err(|e| e.to_string())?;
    let mut mark_count = 0;
    for node in tree.root_element().descendants().skip(1) {
        if node.is_text() { continue; }
        require(node.is_element(), "legacy history text contains non-text structure")?;
        let name = node.tag_name().name();
        require(matches!(name, "strong" | "bold" | "em" | "italic" | "s" | "strike" | "code" | "a" | "link"),
            "legacy history text contains unsupported markup")?;
        for attribute in node.attributes() {
            require(matches!(name, "a" | "link") && (attribute.name() == "href"
                || (native.is_some() && matches!(attribute.name(),"class"|"rel"|"target"))),
                "legacy history text contains unsupported mark attributes")?;
        }
        mark_count += 1;
    }
    require(mark_count > 0, "legacy history text has no inline marks")?;
    let parsed = super::super::content_parse::parse_write_content_for_operation(
        &wrapped, Some("xml"), "legacy-history-text")?;
    require(parsed.warnings.is_empty(), "legacy history marked text cannot be decoded")?;
    if let Some(native) = native {
        // Flat BlockSnapshot marks only expose href. Compare the full native
        // text/mark runs too, so rel/target/class cannot disappear behind that
        // intentionally narrower projection. Coalesce adjacent equal-mark text
        // after separately validated atoms have been removed for this witness.
        fn runs(node: &Value, output: &mut Vec<(String,Value)>) {
            if node["type"] == "text" {
                let text = node["text"].as_str().unwrap_or("");
                let marks = node.get("marks").cloned().unwrap_or(json!([]));
                if let Some((previous, _)) = output.last_mut().filter(|(_,m)|*m == marks) {
                    previous.push_str(text);
                } else { output.push((text.to_string(),marks)); }
            } else if let Some(children) = node["content"].as_array() {
                for child in children { runs(child,output); }
            }
        }
        let mut source_runs = Vec::new(); let mut native_runs = Vec::new();
        runs(&parsed.tiptap_json,&mut source_runs); runs(native,&mut native_runs);
        require(source_runs == native_runs,"legacy full text/mark attributes changed")?;
    }
    let projection = super::super::projection::materialize_tiptap_json(&parsed.tiptap_json, "legacy-history");
    let blocks: Vec<crate::document_types::BlockSnapshot> =
        serde_json::from_value(projection.blocks_json).map_err(|e| e.to_string())?;
    require(blocks.len() == 1, "legacy history marked text changed block shape")?;
    let marks = |value: &crate::document_types::BlockSnapshot| -> Result<Vec<String>, String> {
        let mut rows = Vec::new();
        for mark in &value.marks {
            let mut row = serde_json::to_value(mark).map_err(|e| e.to_string())?;
            row.as_object_mut().ok_or("invalid native mark")?.remove("id");
            rows.push(serde_json::to_string(&row).map_err(|e| e.to_string())?);
        }
        rows.sort();
        Ok(rows)
    };
    Ok(blocks[0].content == block.content && marks(&blocks[0])? == marks(block)?)
}

// Cloud-1 walks through listItem wrappers and indexes their child paragraphs.
// Garden's flat projection instead indexes the listItem itself. Validate the
// source projection against the retained XML AND the nested native tree; do
// not demand that these two intentionally different flat views have equal IDs.
fn validate_legacy_history_blocks(xml: &str, source: &[Value], native: &Value) -> Result<(), String> {
    let wrapped = format!("<doc>{xml}</doc>");
    let document = roxmltree::Document::parse(&wrapped).map_err(|e| e.to_string())?;
    fn walk<'a, 'input>(node: roxmltree::Node<'a, 'input>, rows: &mut Vec<roxmltree::Node<'a, 'input>>) -> Result<(), String> {
        for child in node.children() {
            if child.is_text() {
                require(child.text().unwrap_or("").trim().is_empty(), "unindexed legacy structural text")?;
                continue;
            }
            require(child.is_element(), "unsupported legacy history structure")?;
            match child.tag_name().name() {
                "listItem" | "UNDEFINED" => walk(child, rows)?,
                "paragraph" | "heading" | "codeBlock" | "blockquote" | "horizontalRule" => rows.push(child),
                _ => return Err("unsupported legacy history block structure".into()),
            }
        }
        Ok(())
    }
    fn index_nodes<'a>(node: &'a Value, found: &mut BTreeMap<&'a str, Vec<&'a Value>>) {
        if let Some(id) = node["attrs"]["data-block-id"].as_str() {
            found.entry(id).or_default().push(node);
        }
        if let Some(children) = node["content"].as_array() {
            for child in children { index_nodes(child, found); }
        }
    }
    fn without_atoms(node: &Value, atoms: &mut Vec<Value>) -> Value {
        let mut result = node.clone();
        if let Some(children) = node["content"].as_array() {
            result["content"] = json!(children.iter().filter_map(|child| {
                if matches!(child["type"].as_str(),Some("wikilink"|"tagChip"|"footnote")) {
                    atoms.push(child.clone()); None
                } else { Some(without_atoms(child,atoms)) }
            }).collect::<Vec<_>>());
        }
        result
    }
    let same_attribute = |value: &Value, text: &str| -> bool {
        match value {
            Value::String(v) => v == text,
            Value::Bool(v) => text == if *v { "true" } else { "false" },
            Value::Number(v) => legacy_decimal_attribute(&v.to_string())
                .zip(legacy_decimal_attribute(text)).is_some_and(|(a,b)| a == b),
            _ => false,
        }
    };
    let mut rows = Vec::new();
    walk(document.root_element(), &mut rows)?;
    require(rows.len() == source.len(), "legacy history source block coverage mismatch")?;
    let mut native_by_id = BTreeMap::new();
    index_nodes(native, &mut native_by_id);
    let mut structural_ids = BTreeSet::new();
    for element in document.descendants().filter(|n| n.is_element()) {
        let Some(id) = element.attribute("data-block-id") else { continue; };
        require(structural_ids.insert(id),"duplicate legacy structural identity")?;
        let found = native_by_id.get(id).map(Vec::as_slice).unwrap_or(&[]);
        require(found.len() == 1 && found[0]["type"] == element.tag_name().name(),"legacy structural identity lost in native tree")?;
        for attribute in element.attributes().filter(|a| a.name() != "data-block-id") {
            require(same_attribute(&found[0]["attrs"][attribute.name()],attribute.value()),"legacy structural attribute changed in native tree")?;
        }
    }
    let mut ids = BTreeSet::new();
    for (index, (element, row)) in rows.iter().zip(source).enumerate() {
        let id = string(row, "id")?;
        require(ids.insert(id), "duplicate source history block identity")?;
        require(element.attribute("data-block-id") == Some(id)
            && row["type"] == element.tag_name().name()
            && row["parent_id"].is_null()
            && row["index"].as_u64() == Some(index as u64)
            && row["order"].as_u64() == Some(index as u64)
            && row["collapsed"] == false,
            "legacy history block identity/order/type mismatch")?;
        let empty = serde_json::Map::new();
        let properties = match row.get("properties") {
            None => &empty,
            Some(value) => value.as_object().ok_or("invalid legacy block properties")?,
        };
        require(properties.len() == element.attributes().filter(|a| a.name() != "data-block-id").count(),
            "legacy history property coverage mismatch")?;
        let found = native_by_id.get(id).map(Vec::as_slice).unwrap_or(&[]);
        require(found.len() == 1 && found[0]["type"] == row["type"],
            "legacy history block absent/ambiguous in native tree")?;
        for (key, value) in properties {
            require(element.attribute(key.as_str()).is_some_and(|text| same_attribute(value,text)),
                &format!("legacy history XML property mismatch: {}",json!({"blockId":id,"property":key,
                    "source":value,"xml":element.attribute(key.as_str())})))?;
            let actual = &found[0]["attrs"][key];
            require(actual == value || (actual.is_number() && value.is_number()
                && legacy_decimal_attribute(&actual.to_string()).zip(legacy_decimal_attribute(&value.to_string())).is_some_and(|(a,b)| a == b)),
                &format!("legacy history native property mismatch: {}",json!({"blockId":id,"property":key,
                    "source":value,"xml":element.attribute(key.as_str()),"native":actual})))?;
        }
        let mut native_atoms = Vec::new();
        let text_only = without_atoms(found[0],&mut native_atoms);
        let source_atoms = element.descendants().filter(|n| n.is_element()
            && matches!(n.tag_name().name(),"wikilink"|"tagChip"|"footnote")).collect::<Vec<_>>();
        require(source_atoms.len() == native_atoms.len(),"legacy inline atom population changed")?;
        for (source_atom,native_atom) in source_atoms.iter().zip(&native_atoms) {
            require(source_atom.parent() == Some(*element) && source_atom.children().next().is_none()
                && native_atom["type"] == source_atom.tag_name().name()
                && native_atom.get("content").is_none() && native_atom.get("marks").is_none(),
                "legacy inline atom structure changed")?;
            let attrs = native_atom["attrs"].as_object().ok_or("legacy inline atom attributes missing")?;
            require(attrs.len() == source_atom.attributes().len()
                && source_atom.attributes().all(|attr|attrs.get(attr.name()).is_some_and(|value|same_attribute(value,attr.value()))),
                "legacy inline atom attributes changed")?;
        }
        let projection = super::super::projection::materialize_tiptap_json(
            &json!({"type":"doc", "content":[text_only]}), "legacy-history");
        let blocks: Vec<crate::document_types::BlockSnapshot> =
            serde_json::from_value(projection.blocks_json).map_err(|e| e.to_string())?;
        require(blocks.len() == 1 && blocks[0].id == id, "legacy history isolated block mismatch")?;
        let text = string(row, "text")?;
        require((blocks[0].content == text && blocks[0].marks.is_empty()) || legacy_history_marked_text_matches(text, &blocks[0], Some(&text_only))?,
            "history text/XML mismatch")?;
    }
    Ok(())
}

// XML serializes numeric Y.Map attributes as decimal strings. Ignore only
// trailing fractional zero spelling, never round through floating point.
pub(super) fn legacy_decimal_attribute(value: &str) -> Option<&str> {
    let digits = value.strip_prefix('-').unwrap_or(value);
    let mut split = digits.split('.');
    let whole = split.next()?;
    if whole.is_empty() || !whole.bytes().all(|b| b.is_ascii_digit()) { return None; }
    match split.next() {
        None => Some(value),
        Some(fraction) if !fraction.is_empty() && fraction.bytes().all(|b| b.is_ascii_digit()) && split.next().is_none() => {
            Some(value.trim_end_matches('0').trim_end_matches('.'))
        }
        _ => None,
    }
}

fn original_effective_mime<'a>(entry: &'a Value, manifest: &Value) -> Result<&'a str, String> {
    if manifest["transformation"] == V23 && entry["ownerKind"] == "image"
        && entry.get("mimeType") == Some(&Value::Null) {
        // This is native opaque-download metadata, never an inferred source
        // MIME or image format. Source null and exact bytes remain in custody.
        return Ok("application/octet-stream");
    }
    string(entry, "mimeType")
}

// The semantic index deliberately contains only available payloads. Account for
// the wider retained Dynamo inventory separately; missing bytes are never a
// native snapshot, empty payload, revision, or restore grant.
fn validate_history_custody_document_id(manifest: &Value, graph: &str, record: &Value) -> Result<(), String> {
    let original = string(&record["doc_id"], "S")?;
    if source_id(original).is_ok() { return Ok(()); }
    // This identity belongs to retained source evidence, not a native path.
    // Only the same declared mapping already checked against the semantic
    // history index permits its original spelling to remain in raw custody.
    let mapped = manifest["compatibilityTransform"]["identifierMap"][original]
        .as_str().ok_or("raw history identity has no declared compatible mapping")?;
    source_id(mapped)?;
    require(history_attribute_matches(manifest, graph, "doc_id", Some(&record["doc_id"]),
        &json!({"S":mapped})), "raw history identity mapping is not admitted")
}

fn prepare_history_availability(members: &Members, manifest: &Value, graph: &str, user: &str) -> Result<(Vec<Value>, Value), String> {
    if manifest["transformation"] != V23 { return Ok((Vec::new(), Value::Null)); }
    let custody = parse_json(member(members, "source-custody/index.json")?)?;
    require(custody["userId"] == user && custody["graphId"] == graph, "history custody scope mismatch")?;
    let mut available = BTreeMap::new();
    for raw in array(manifest, "sourceRecords")? {
        let record = parse_json(string(raw, "record_json")?.as_bytes())?;
        available.insert(string(&record["snapshot_id"], "S")?.to_string(), record);
    }
    let mut retained = BTreeMap::new();
    for raw in array(&custody, "dynamoRecords")? {
        require(raw["table"] == "mnemosyne-document-snapshots", "history custody table mismatch")?;
        let record = &raw["record"];
        let id = string(&record["snapshot_id"], "S")?;
        let doc = string(&record["doc_id"], "S")?;
        source_id(id)?; validate_history_custody_document_id(manifest, graph, record)?;
        require(record["owner_user_id"]["S"] == user && record["graph_id"]["S"] == graph
            && record["doc_key"]["S"] == format!("{graph}#{doc}"), "history custody identity mismatch")?;
        require(retained.insert(id.to_string(), record.clone()).is_none(), "duplicate history custody record")?;
    }
    for (id, record) in &available {
        require(retained.get(id) == Some(record), "available history custody mismatch")?;
    }
    let Some(recovery) = custody.get("documentHistoryRecovery") else {
        require(retained == available, "unaccounted unavailable history")?;
        let index=parse_json(member(members,"history/documents/index.json")?)?;
        let mut entries=Vec::new();
        for row in index.as_array().ok_or("history index invalid")? {
            let id=string(row,"snapshot_id")?;let path=string(row,"member")?;
            let record=retained.get(id).ok_or("history recovery record missing")?;
            entries.push(json!({"snapshotId":id,"documentId":row["doc_id"],
                "recordSha256":archive_sha256(&json_bytes(record)?),"disposition":"semantic-history-present",
                "sourceMember":path,"sourceMemberSha256":archive_sha256(member(members,path)?)}));
        }
        require(entries.len()==retained.len(),"history recovery inventory incomplete")?;
        return Ok((Vec::new(),json!({"schema":"cloud1-document-history-availability.v1",
            "sourceUserId":user,"sourceGraphId":graph,"rawSourceRetained":true,
            "indexMember":"source-custody/index.json","indexSha256":archive_sha256(member(members,"source-custody/index.json")?),
            "retainedMetadataCount":retained.len(),"availablePayloadCount":available.len(),
            "unavailablePayloadCount":0,"entries":entries})));
    };
    require(recovery["mode"] == "available-payloads-only-v1"
        && recovery["unavailablePayloads"] == "not-substituted-not-fabricated", "history recovery mode mismatch")?;
    let mut seen = BTreeSet::new();
    let mut result = Vec::new();
    let mut partition = Vec::new();
    let index = parse_json(member(members, "history/documents/index.json")?)?;
    for row in array(recovery, "snapshots")? {
        let id = string(row, "snapshotId")?;
        let doc = string(row, "documentId")?;
        let record = retained.get(id).ok_or("history recovery record missing")?;
        let key = format!("users/{user}/graphs/{graph}/document-snapshots/{doc}/{id}.json");
        require(seen.insert(id.to_string()) && record["doc_id"]["S"] == doc
            && row["key"] == key && row["bucket"] == "mnemosyne-dev-prod-crdt-state"
            && row["recordSha256"] == archive_sha256(&json_bytes(record)?), "history recovery source mismatch")?;
        if available.contains_key(id) {
            require(row["disposition"] == "semantic-history-present", "available history marked unavailable")?;
            let indexed = index.as_array().ok_or("history index must be an array")?.iter().find(|entry| entry["snapshot_id"] == id)
                .ok_or("available history absent from semantic index")?;
            let path = string(indexed, "member")?;
            let mut evidence = row.clone();
            evidence["sourceMember"] = json!(path);
            evidence["sourceMemberSha256"] = json!(archive_sha256(member(members, path)?));
            partition.push(evidence);
        } else {
            require(row["disposition"] == "custody-only-payload-unavailable", "unavailable history disposition mismatch")?;
            require(!array(&custody, "objects")?.iter().any(|object| object["bucket"] == row["bucket"] && object["key"] == key)
                && !manifest["sourceObjects"].as_object().ok_or("source objects missing")?.values().any(|object| object["key"] == key),
                "unavailable history has captured payload")?;
            let mut evidence = row.clone();
            evidence["sourceUserId"] = json!(user);
            evidence["sourceGraphId"] = json!(graph);
            evidence["nativeSnapshotCreated"] = json!(false);
            evidence["semanticRestoreAuthority"] = json!(false);
            evidence["rawSourceRetained"] = json!(true);
            evidence["indexMember"] = json!("source-custody/index.json");
            evidence["indexSha256"] = json!(archive_sha256(member(members, "source-custody/index.json")?));
            result.push(evidence);
            partition.push(row.clone());
        }
    }
    require(seen == retained.keys().cloned().collect(), "history recovery inventory incomplete")?;
    result.sort_by(|a,b| a["snapshotId"].as_str().cmp(&b["snapshotId"].as_str()));
    partition.sort_by(|a,b| a["snapshotId"].as_str().cmp(&b["snapshotId"].as_str()));
    let accounting = json!({"schema":"cloud1-document-history-availability.v1",
        "sourceUserId":user,"sourceGraphId":graph,"rawSourceRetained":true,
        "indexMember":"source-custody/index.json","indexSha256":archive_sha256(member(members,"source-custody/index.json")?),
        "retainedMetadataCount":retained.len(),"availablePayloadCount":available.len(),
        "unavailablePayloadCount":result.len(),"entries":partition});
    Ok((result, accounting))
}

fn history_owner_partition(members: &Members, manifest: &Value, history: Vec<History>,
    mut legacy: Vec<Value>, current: &BTreeSet<String>, deleted: &BTreeSet<String>)
    -> Result<(Vec<History>,Vec<Value>),String> {
    let index=parse_json(member(members,"history/documents/index.json")?)?;
    let mut native=Vec::new();
    for item in history {
        if current.contains(&item.meta.document_id) || deleted.contains(&item.meta.document_id) {
            native.push(item);
        }else{
            require(manifest["transformation"]==V23,"history owner is neither live nor explicitly deleted")?;
            let entry=index.as_array().ok_or("history index missing")?.iter()
                .find(|row|row["snapshot_id"]==item.meta.snapshot_id).ok_or("history index entry missing")?;
            legacy.push(history_source_entry(members,manifest,entry,"legacy-read-only",
                "source-document-not-current-or-deleted",None)?);
        }
    }
    Ok((native,legacy))
}

fn history_source_entry(members: &Members, manifest: &Value, entry: &Value,
    status: &str, reason: &str, diagnostic: Option<String>) -> Result<Value,String> {
    let id=string(entry,"snapshot_id")?; let path=string(entry,"member")?;
    let record=array(manifest,"sourceRecords")?.iter().find(|row| {
        row["key_json"].as_str().and_then(|s|serde_json::from_str::<Value>(s).ok())
            .is_some_and(|key|key["snapshot_id"]["S"]==id)
    }).ok_or("history source record missing")?;
    Ok(json!({"sourceUserId":manifest["source"]["userId"],"sourceGraphId":entry["graph_id"],
        "documentId":entry["doc_id"],"snapshotId":id,"sourceMetadata":entry,
        "sourceMember":path,"sourceMemberSha256":archive_sha256(member(members,path)?),
        "sourceByteLength":member(members,path)?.len(),"sourceRecordSha256":record["record_sha256"],
        "sourceIndexMember":"history/documents/index.json",
        "sourceIndexSha256":archive_sha256(member(members,"history/documents/index.json")?),
        "status":status,"reasonCode":reason,"diagnostic":diagnostic,
        "nativeSnapshotCreated":status=="native-interpreted","restoreSupported":status=="native-interpreted",
        "readOnly":status!="native-interpreted","nativeRestorable":status=="native-interpreted",
        "originalYjsRecovered":false,"rawSourceRetained":true}))
}

fn history_fidelity(native: usize, legacy: usize, unavailable: usize) -> &'static str {
    match (native,legacy,unavailable) {
        (0,0,0)=>"no-source-history",
        (_,0,0)=>"qualified-native-interpretation",
        (0,0,_)=>"source-history-unavailable",
        (_,0,_)=>"disclosed-unavailable-history",
        (_,_,0)=>"disclosed-read-only-legacy-history",
        _=>"disclosed-legacy-and-unavailable-history",
    }
}

fn history_disposition(members: &Members, manifest: &Value, native: &[History],
    legacy: &[Value], unavailable: &[Value]) -> Result<Value,String> {
    let index=parse_json(member(members,"history/documents/index.json")?)?;
    let mut entries=legacy.to_vec();
    for item in native {
        let source=index.as_array().ok_or("history index invalid")?.iter()
            .find(|row|row["snapshot_id"]==item.meta.snapshot_id).ok_or("history index entry missing")?;
        entries.push(history_source_entry(members,manifest,source,"native-interpreted",
            "qualified-exported-history-interpretation",None)?);
    }
    for row in unavailable {
        let mut entry=row.clone();
        entry["status"]=json!("source-payload-unavailable");
        entry["reasonCode"]=json!("source-payload-unavailable");
        entry["restoreSupported"]=json!(false);
        entry["readOnly"]=json!(true);
        entry["nativeRestorable"]=json!(false);
        entries.push(entry);
    }
    entries.sort_by(|a,b|a["snapshotId"].as_str().cmp(&b["snapshotId"].as_str()));
    let ids=entries.iter().map(|row|string(row,"snapshotId")).collect::<Result<BTreeSet<_>,_>>()?;
    require(ids.len()==entries.len(),"history disposition duplicate identity")?;
    Ok(json!({"schema":"cloud1-document-history-disposition.v1",
        "sourceUserId":manifest["source"]["userId"],"sourceGraphId":manifest["source"]["graphId"],
        "nativeInterpretedCount":native.len(),"legacyReadOnlyCount":legacy.len(),
        "unavailablePayloadCount":unavailable.len(),"retainedMetadataCount":entries.len(),
        "fidelity":history_fidelity(native.len(),legacy.len(),unavailable.len()),
        "entries":entries}))
}

fn interpret_history_body(body: &Value, manifest: &Value, doc: &str, id: &str)
    -> Result<(String, Vec<crate::document_types::BlockSnapshot>), String> {
        let source_blocks = array(&body, "blocks")?;
        let xml = string(&body, "tiptap_xml")?;
        let legacy_blocks = manifest["transformation"] == V23
            && source_blocks.iter().any(|row| row.get("index").is_some());
        let mut native_xml = if legacy_blocks { legacy_history_export_xml(xml,source_blocks)? } else { xml.to_string() };
        let mut parsed =
            super::super::content_parse::parse_write_content_for_operation(&native_xml, Some("xml"), id)?;
        if !parsed.warnings.is_empty() {
            require(
                parsed.warnings.len() == 1
                    && parsed.warnings[0]
                        .starts_with("Content parse fallback: malformed entity reference"),
                "history XML cannot be faithfully decoded",
            )?;
            native_xml = escape_legacy_bare_xml_ampersands(xml);
            require(native_xml != xml, "history XML repair made no progress")?;
            parsed = super::super::content_parse::parse_write_content_for_operation(
                &native_xml,
                Some("xml"),
                id,
            )?;
        }
        require(
            parsed.warnings.is_empty() && parsed.source_format == "xml",
            "history XML cannot be faithfully decoded",
        )?;
        let projection =
            super::super::projection::materialize_tiptap_json(&parsed.tiptap_json, doc);
        let blocks: Vec<crate::document_types::BlockSnapshot> =
            serde_json::from_value(projection.blocks_json).map_err(|e| e.to_string())?;
        if legacy_blocks {
            validate_legacy_history_blocks(&native_xml, source_blocks, &parsed.tiptap_json)?;
        } else {
        let mut block_ids = BTreeSet::new();
        for source_block in source_blocks {
            let block_id = string(source_block, "id")?;
            require(
                block_ids.insert(block_id),
                "duplicate source history block identity",
            )?;
            let block = blocks
                .iter()
                .find(|b| b.id == block_id)
                .ok_or_else(|| format!("history block missing from XML projection (legacy type {})",
                    source_block.get("type").and_then(Value::as_str).filter(|s| matches!(*s,
                        "paragraph"|"heading"|"codeBlock"|"blockquote"|"bulletList"|"orderedList"|"taskList"|"horizontalRule")).unwrap_or("other")))?;
            if let Some(text) = source_block.get("text").and_then(Value::as_str) {
                require(block.content == text || (manifest["transformation"] == V23
                    && legacy_history_marked_text_matches(text, block, None)?), "history text/XML mismatch")?;
            } else {
                let native: crate::document_types::BlockSnapshot =
                    serde_json::from_value(source_block.clone())
                        .map_err(|_| "unsupported source history block shape")?;
                require(
                    json_bytes(&native)? == json_bytes(block)?,
                    "history block/XML projection mismatch",
                )?;
            }
        }
        require(
            blocks.len() == source_blocks.len(),
            "history XML has unindexed blocks",
        )?;
        }
    Ok((native_xml, blocks))
}

fn prepare_history(
    members: &Members,
    manifest: &Value,
    graph: &str,
    user: &str,
) -> Result<Vec<History>, String> {
    let (native, legacy) = prepare_history_partition(members, manifest, graph, user, false)?;
    require(legacy.is_empty(), "strict history unexpectedly retained legacy record")?;
    Ok(native)
}

fn history_attribute_matches(manifest: &Value, graph: &str, key: &str, raw: Option<&Value>, expected: &Value) -> bool {
    if raw == Some(expected) { return true; }
    let rule = &manifest["compatibilityTransform"];
    if rule["rule"] != "vera-20260920-standard-compatibility-v1" || rule["originalCustodyUnchanged"] != true {
        return false;
    }
    let Some(value) = raw.and_then(|v| v.get("S")).and_then(Value::as_str) else { return false; };
    let old = match key {
        "doc_id" => value,
        "doc_key" => match value.strip_prefix(&format!("{graph}#")) { Some(id) => id, None => return false },
        _ => return false,
    };
    let Some(mapped) = rule["identifierMap"][old].as_str() else { return false; };
    if source_id(mapped).is_err() { return false; }
    *expected == json!({"S": if key == "doc_key" { format!("{graph}#{mapped}") } else { mapped.to_string() }})
}

fn check_history_record(manifest: &Value, graph: &str, entry: &Value, record: &Value) -> Result<(), String> {
    for (key, value) in entry.as_object().ok_or("history entry must be an object")? {
        if key == "member" || (key == "label" && value.is_null() && record.get(key).is_none()) { continue; }
        let attribute = match value {
            Value::Bool(v) => json!({"BOOL":v}), Value::Null => json!({"NULL":true}),
            Value::Number(v) if v.is_i64() || v.is_u64() => json!({"N":v.to_string()}),
            Value::String(v) => json!({"S":v}), _ => return Err("unsupported history index field type".into()),
        };
        require(history_attribute_matches(manifest, graph, key, record.get(key), &attribute),
            "history metadata differs from retained Dynamo record")?;
    }
    Ok(())
}

fn prepare_history_partition(members: &Members, manifest: &Value, graph: &str, user: &str,
    allow_legacy: bool) -> Result<(Vec<History>, Vec<Value>), String> {
    let index = parse_json(member(members, "history/documents/index.json")?)?;
    let index = index.as_array().ok_or("history index must be an array")?;
    let mut records = BTreeMap::new();
    let mut raw_bytes = 0usize;
    for raw in array(manifest, "sourceRecords")? {
        require(
            raw["table_name"] == "mnemosyne-document-snapshots",
            "unsupported source record table",
        )?;
        let text = string(raw, "record_json")?;
        let key_text = string(raw, "key_json")?;
        raw_bytes += text.len() + key_text.len();
        require(raw_bytes <= 16 * 1024 * 1024, "source record byte budget")?;
        require(
            raw["record_sha256"] == archive_sha256(text.as_bytes()),
            "raw Dynamo record hash mismatch",
        )?;
        let record = parse_json(text.as_bytes())?;
        let id = string(&record["snapshot_id"], "S")?;
        source_id(id)?;
        require(
            parse_json(key_text.as_bytes())? == json!({"snapshot_id":{"S":id}}),
            "Dynamo record key mismatch",
        )?;
        require(
            records.insert(id.to_string(), record).is_none(),
            "duplicate raw history record",
        )?;
    }
    let mut paths = set(&["history/documents/index.json"]);
    let mut ids = BTreeSet::new();
    let mut result = Vec::new();
    let mut legacy = Vec::new();
    // Reject metadata drift before interpreting potentially large historical bodies.
    for entry in index {
        let record = records.get(string(entry, "snapshot_id")?).ok_or("history raw record missing")?;
        check_history_record(manifest, graph, entry, record)?;
    }
    for entry in index {
        let id = string(entry, "snapshot_id")?;
        source_id(id)?;
        let doc = string(entry, "doc_id")?;
        source_id(doc)?;
        require(ids.insert(id.to_string()), "duplicate snapshot identity")?;
        require(
            entry["owner_user_id"] == user
                && entry["graph_id"] == graph
                && entry["doc_key"] == format!("{graph}#{doc}"),
            "foreign history owner/key",
        )?;
        let path = string(entry, "member")?;
        require(
            path.starts_with("history/documents/")
                && path != "history/documents/index.json"
                && paths.insert(path.into()),
            "duplicate/noncanonical history member",
        )?;
        require(
            manifest["sourceObjects"][path]["key"]
                == format!("users/{user}/graphs/{graph}/document-snapshots/{doc}/{id}.json"),
            "history source key mismatch",
        )?;
        let body = parse_json(member(members, path)?)?;
        let created = string(entry, "created_at")?;
        // Native history uses unsigned epoch-millisecond strings, not RFC3339.
        // Exact source spelling and any sub-ms precision remain in custody.
        let native_created = checked_timestamp(created)?.to_string();
        require(
            body["snapshot_id"] == id && body["created_at"] == created,
            "history body identity/time mismatch",
        )?;
        let tier = string(entry, "tier")?;
        require(
            crate::document_history_store::HISTORY_TIERS.contains(&tier)
                && number(entry, "snapshot_count")? > 0,
            "history tier/count invalid",
        )?;
        let manual = entry
            .get("is_manual")
            .and_then(Value::as_bool)
            .ok_or("history manual flag missing")?;
        let label = match entry.get("label") {
            Some(Value::Null) => None,
            Some(Value::String(s)) => Some(s.clone()),
            _ => return Err("history label missing/invalid".into()),
        };
        let delta = |key: &str| -> Result<i64, String> {
            i64::try_from(number(entry, key)?).map_err(|e| e.to_string())
        };
        let meta = LocalDocumentSnapshotMeta {
            snapshot_id: id.into(),
            graph_id: graph.into(),
            document_id: doc.into(),
            is_manual: manual,
            document_revision: None,
            tier: tier.into(),
            snapshot_count: number(entry, "snapshot_count")?,
            chars_added: delta("chars_added")?,
            chars_removed: delta("chars_removed")?,
            blocks_added: delta("blocks_added")?,
            blocks_removed: delta("blocks_removed")?,
            blocks_modified: delta("blocks_modified")?,
            created_at: native_created.clone(),
            label,
        };
        let (native_xml, blocks) = match interpret_history_body(&body, manifest, doc, id) {
            Ok(value) => value,
            Err(error) if allow_legacy && manifest["transformation"] == V23 => {
                legacy.push(history_source_entry(members, manifest, entry, "legacy-read-only",
                    "native-interpretation-unsupported", Some(error))?);
                continue;
            }
            Err(error) => return Err(error),
        };
        // Cloud-1 bodies need not contain a historical title. The ID is an
        // explicit display fallback, never the current title mislabeled as old.
        let title = body
            .get("title")
            .and_then(Value::as_str)
            .unwrap_or(doc)
            .to_string();
        let payload = LocalDocumentSnapshotPayload {
            snapshot_id: id.into(),
            graph_id: graph.into(),
            document_id: doc.into(),
            title,
            created_at: native_created,
            blocks,
            tiptap_xml: native_xml,
        };
        result.push(History { meta, payload });
    }
    require(
        ids == records.keys().cloned().collect(),
        "raw history inventory incomplete",
    )?;
    exact_class(manifest, "document-history", paths)?;
    Ok((result, legacy))
}

fn prepare_graph_history(
    members: &Members,
    manifest: &Value,
    source_manifest: &GraphArchiveManifest,
    graph: &str,
) -> Result<Vec<GraphHistory>, String> {
    use crate::time_travel_types::*;
    let mapping = parse_json(member(members, "history/graph/version-members.json")?)?;
    let scoped = manifest["transformation"] != V21;
    let mut versions = BTreeMap::new();
    if scoped {
        for entry in mapping
            .as_array()
            .ok_or("scoped version map must be an array")?
        {
            let key = string(entry, "key")?.to_string();
            let version = string(entry, "version_id")?.to_string();
            require(
                versions
                    .insert((key, version), string(entry, "member")?.to_string())
                    .is_none(),
                "duplicate scoped version mapping",
            )?;
        }
    } else {
        for (version, path) in mapping
            .as_object()
            .ok_or("v2.1 version map must be an object")?
        {
            versions.insert(
                (String::new(), version.clone()),
                path.as_str()
                    .ok_or("invalid historical member path")?
                    .to_string(),
            );
        }
    }
    let mut used_versions = BTreeSet::new();
    let mut paths = set(&["history/graph/version-members.json"]);
    let mut point_ids = BTreeSet::new();
    let mut result = Vec::new();
    let mut version_keys = BTreeMap::new();
    let mut projections = HistoryProjectionCache::new();
    let mut expanded_history_bytes = 0usize;
    let mut resolve = |reference: &Value| -> Result<(String, &[u8]), String> {
        let key = string(reference, "key")?;
        let version = string(reference, "version_id")?;
        let identity = (
            if scoped {
                key.to_string()
            } else {
                String::new()
            },
            version.to_string(),
        );
        if let Some(prior) = version_keys.insert(identity.clone(), key.to_string()) {
            require(prior == key, "v2.1 ambiguous VersionId across source keys")?;
        }
        let path = versions
            .get(&identity)
            .ok_or("historical source version unresolved")?;
        require(
            manifest["sourceObjects"][path]["key"] == key
                && manifest["sourceObjects"][path]["version_id"] == version,
            "historical bytes resolve to wrong source key/version",
        )?;
        let bytes = member(members, path)?;
        expanded_history_bytes = expanded_history_bytes
            .checked_add(bytes.len())
            .ok_or("historical expansion overflow")?;
        require(
            expanded_history_bytes <= HISTORY_EXPANSION_LIMIT,
            "historical version reuse exceeds native expansion budget",
        )?;
        if !reference["size_bytes"].is_null() {
            require(
                number(reference, "size_bytes")? == bytes.len() as u64,
                "historical reference size mismatch",
            )?;
        }
        used_versions.insert(identity);
        paths.insert(path.clone());
        Ok((path.clone(), bytes))
    };
    for path in class_paths(manifest, "graph-history")? {
        if !path.starts_with("history/graph/") || path == "history/graph/version-members.json" {
            continue;
        }
        let source = parse_json(member(members, &path)?)?;
        let id = string(&source, "snapshot_id")?;
        source_id(id)?;
        require(
            point_ids.insert(id.to_string()) && path == format!("history/graph/{id}.json"),
            "duplicate/noncanonical graph snapshot",
        )?;
        require(
            source["format_version"].as_u64() == Some(1)
                && source["graph_id"] == graph
                && source["user_id"] == source_manifest.source_user_id,
            "graph history format/owner mismatch",
        )?;
        if scoped {
            require(
                manifest["sourceObjects"][&path]["key"]
                    == format!(
                        "users/{}/graphs/{graph}/snapshots/{id}.json",
                        source_manifest.source_user_id
                    ),
                "graph history source key mismatch",
            )?;
        }
        let created = string(&source, "created_at")?;
        let time = checked_timestamp(created)?;
        require(
            source["workspace"]["key"]
                == format!(
                    "users/{}/graphs/{graph}/workspace.yjs",
                    source_manifest.source_user_id
                ),
            "historical workspace reference mismatch",
        )?;
        let (_, workspace_bytes) = resolve(&source["workspace"])?;
        let workspace = parsed_doc(&workspace_bytes, "historical workspace")?;
        let historical_documents = roots(&workspace, "documents")?;
        rewrite_graph_archive_workspace(&workspace, source_manifest, graph);
        let snapshot =
            super::super::workspace_ops::materialize_workspace_snapshot_json(graph, &workspace)?;
        let workspace_bytes = encode_full_state(&workspace);
        let mut docs = Vec::new();
        let mut refs = Vec::new();
        let mut payloads = Vec::new();
        let mut doc_ids = BTreeSet::new();
        let mut id_title_fallbacks = Vec::new();
        for source_doc in array(&source, "documents")? {
            let doc_id = string(source_doc, "doc_id")?;
            source_id(doc_id)?;
            require(
                doc_ids.insert(doc_id.to_string()),
                "graph history document inventory mismatch",
            )?;
            require(
                source_doc["ref"]["key"]
                    == format!(
                        "users/{}/graphs/{graph}/documents/{doc_id}.yjs",
                        source_manifest.source_user_id
                    ),
                "historical document key mismatch",
            )?;
            let (member_path, bytes) = resolve(&source_doc["ref"])?;
            let content = history_projection(&mut projections, bytes, doc_id)?;
            let workspace_title = snapshot["documents"]
                .as_array()
                .and_then(|rows| rows.iter().find(|r| r["id"] == doc_id))
                .and_then(|r| r["title"].as_str());
            let title = match workspace_title {
                Some(title) => title.to_string(),
                // Cloud-1 enumerated stored document keys, not workspace
                // membership. Retain that exact orphan payload; its ID is an
                // explicit display fallback, not a manufactured source title.
                None if !historical_documents.contains_key(doc_id) => {
                    id_title_fallbacks.push(doc_id.to_string());
                    doc_id.to_string()
                }
                None => return Err("historical title missing".into()),
            };
            require(
                crate::ids::normalize_stored_title(&title)? == title,
                "historical title would be normalized",
            )?;
            // An adapter identity, NOT a manufactured source document-history
            // event: referenced only by this imported graph checkpoint.
            let snapshot_id = format!(
                "source-graph-{}",
                archive_sha256(&json_bytes(&json!([id, doc_id, member_path]))?)
            );
            refs.push(DocumentSnapshotRef {
                document_id: doc_id.into(),
                title: title.clone(),
                snapshot_id: snapshot_id.clone(),
                size_bytes: bytes.len() as u64,
                block_count: content.blocks.len() as u64,
                char_count: content.char_count,
                ydoc_bytes_path: Some(format!("documents/{doc_id}.bin")),
            });
            payloads.push(GraphHistoryPayload {
                snapshot_id,
                graph_id: graph.into(),
                document_id: doc_id.into(),
                title,
                created_at: time.to_string(),
                content: Arc::clone(&content),
            });
            docs.push((doc_id.to_string(), Arc::clone(&content.bytes)));
        }
        // Cloud-1 snapshots may omit missing/zero-byte blobs OR include stored
        // documents outside the workspace. Preserve both kinds of testimony,
        // but only exact membership is an executable native restore point.
        // Existing schema 1 refuses restore before any effect.
        let workspace_ids: BTreeSet<String> = historical_documents.keys().cloned().collect();
        let complete = doc_ids == workspace_ids;
        let workspace_only_document_ids = workspace_ids.difference(&doc_ids).cloned().collect();
        let storage_only_document_ids = doc_ids.difference(&workspace_ids).cloned().collect();
        let metadata = RestorePointMetadata {
            folder_count: array(&snapshot, "folders")?.len() as u64,
            document_count: docs.len() as u64,
            artifact_count: array(&snapshot, "artifacts")?.len() as u64,
            wire_count: array(&snapshot, "wires")?.len() as u64,
            workspace_size_bytes: workspace_bytes.len() as u64,
            total_document_size_bytes: docs.iter().map(|(_, b)| b.len() as u64).sum(),
        };
        let native = RestorePointManifest {
            schema_version: if complete {
                RESTORE_POINT_MANIFEST_SCHEMA_VERSION
            } else {
                1
            },
            restore_point_id: id.into(),
            graph_id: graph.into(),
            created_at: time,
            trigger: RestorePointTrigger::Checkpoint,
            label: Some("Imported cloud-1 checkpoint (source trigger unspecified)".into()),
            content_hash_sha256: archive_sha256(member(members, &path)?),
            workspace: WorkspaceSnapshotRef {
                bytes_path: "workspace.bin".into(),
                snapshot_path: "workspace.json".into(),
                size_bytes: workspace_bytes.len() as u64,
            },
            documents: refs,
            metadata,
        };
        result.push(GraphHistory {
            manifest: native,
            workspace: workspace_bytes.to_vec(),
            snapshot,
            documents: docs,
            payloads,
            workspace_only_document_ids,
            storage_only_document_ids,
            id_title_fallbacks,
        });
        // The resolver retains its own mutable reference to paths; add source
        // manifests after it is dropped below.
    }
    drop(resolve);
    for id in point_ids {
        paths.insert(format!("history/graph/{id}.json"));
    }
    require(
        used_versions == versions.keys().cloned().collect(),
        "unreferenced version mapping",
    )?;
    exact_class(manifest, "graph-history", paths)?;
    Ok(result)
}

fn dataset_destination(graph: &str, archive: &str, kind: &str, iri: &str) -> String {
    let identity = format!("{DATASET_CAPABILITY}\0{kind}\0{iri}");
    format!("{}:user:retained-source:{archive}:{}", crate::rdf::graph_subject(graph), archive_sha256(identity.as_bytes()))
}

fn prepare_dataset_partitions(members: &Members, manifest: &Value, graph: &str, user: &str, archive: &str) -> Result<Value, String> {
    if !members.contains_key(DATASET_MEMBER) { return Ok(Value::Null); }
    let witness = parse_json(member(members, DATASET_MEMBER)?)?;
    let custody = parse_json(member(members, "source-custody/index.json")?)?;
    let source_bytes = |source: &Value| -> Result<Vec<u8>, String> {
        let entries: Vec<_> = array(&custody, "objects")?.iter().filter(|entry|
            ["bucket","key","version_id","sha256","byte_length"].iter().all(|key|entry[*key]==source[*key])).collect();
        require(entries.len()==1 && entries[0]["latest"]==true, "partition source custody mismatch")?;
        let raw = member(members, string(entries[0], "member")?)?;
        require(source["sha256"]==archive_sha256(raw) && number(source,"byte_length")?==raw.len() as u64,
            "partition source bytes mismatch")?;
        let mut decoder = flate2::bufread::GzDecoder::new(raw);
        let mut output = Vec::new();
        decoder.by_ref().take(MEMBER_LIMIT as u64+1).read_to_end(&mut output).map_err(|e|e.to_string())?;
        require(output.len()<=MEMBER_LIMIT && decoder.into_inner().is_empty(), "partition gzip bounds/trailing data")?;
        Ok(output)
    };
    let catalogue_source = &custody["catalogueSource"];
    let dataset_source = &witness["datasetSource"];
    let key = string(catalogue_source, "key")?;
    let prefix = key.strip_suffix(&format!("/{user}/__meta__.nq.gz")).ok_or("partition catalogue owner layout mismatch")?;
    require(!prefix.is_empty() && dataset_source["bucket"]==catalogue_source["bucket"]
        && dataset_source["key"]==format!("{prefix}/{user}/{graph}.nq.gz"), "partition source owner layout mismatch")?;
    let source_rdf=source_bytes(dataset_source)?;
    let rdf=member(members,"rdf/dataset.nq")?;
    let derived=source_rdf.as_slice()!=rdf;
    if derived {derived_dataset::verify_derived_dataset(&source_rdf,rdf,members,manifest,user,graph)?;}
    let catalogue = source_bytes(catalogue_source)?;
    let meta = format!("urn:mnemosyne:user:{user}:meta");
    let main = format!("urn:mnemosyne:user:{user}:graph:{graph}");
    let owner = format!("urn:mnemosyne:user:{user}:graph:");
    let mut subjects = BTreeSet::new();
    for quad in RdfParser::from_format(RdfFormat::NQuads).for_slice(&catalogue) {
        let quad = quad.map_err(|e|e.to_string())?;
        if matches!(&quad.graph_name, GraphName::NamedNode(name) if name.as_str()==meta) {
            if let NamedOrBlankNode::NamedNode(name)=quad.subject { subjects.insert(name.as_str().to_string()); }
        }
    }
    let mut counts = BTreeMap::<(String,String),u64>::new();
    let mut seen = BTreeSet::new();
    for quad in RdfParser::from_format(RdfFormat::NQuads).for_slice(&rdf) {
        let quad = quad.map_err(|e|e.to_string())?;
        require(seen.insert(quad.to_string()), "duplicate RDF statement testimony")?;
        let identity = match &quad.graph_name {
            GraphName::NamedNode(name)=>("named".to_string(),name.as_str().to_string()),
            GraphName::DefaultGraph=>("default".to_string(),String::new()),
            _=>return Err("blank RDF graph unsupported".into()),
        };
        if identity.0!="named" || identity.1!=main {
            require(matches!(quad.subject,NamedOrBlankNode::NamedNode(_)) && matches!(quad.object,Term::NamedNode(_)|Term::Literal(_)), "side blank nodes or quoted triples unsupported")?;
            if identity.0=="named" { require(identity.1==meta || (identity.1.starts_with(&owner) && subjects.contains(&identity.1)), "retained sibling owner/catalogue witness missing")?; }
        }
        *counts.entry(identity).or_default()+=1;
    }
    let partitions: Vec<Value> = counts.iter().map(|((kind,iri),count)|json!({
        "identity":if kind=="named" {json!({"kind":kind,"iri":iri})} else {json!({"kind":kind})},
        "quadCount":count,"disposition":if kind=="named" && iri==&main {"main-authority"} else {"retained-source-testimony"}
    })).collect();
    let mut expected = json!({"schema":DATASET_CAPABILITY,"captureId":manifest["source"]["capture"]["id"],
        "userId":user,"graphId":graph,"inventorySha256":custody["inventorySha256"],
        "datasetSource":dataset_source,"catalogueSource":catalogue_source,"datasetSha256":archive_sha256(&rdf),
        "destinationAuthority":"not-conferred","partitions":partitions});
    if derived {expected["originalDatasetSha256"]=json!(archive_sha256(&source_rdf));}
    require(witness==expected, "retained dataset partition witness mismatch")?;
    let mut result = expected;
    result["archiveSha256"]=json!(archive);
    let mut destinations = BTreeSet::new();
    for row in result["partitions"].as_array_mut().ok_or("partition array")? {
        let kind = string(&row["identity"], "kind")?;
        let iri = row["identity"]["iri"].as_str().unwrap_or("");
        let destination = if kind=="named" && iri==main { crate::rdf_authority::user_rdf_graph_iri(graph) }
            else { dataset_destination(graph,archive,kind,iri) };
        require(destinations.insert(destination.clone()), "partition mapping collision")?;
        row["destinationGraph"]=json!(destination);
    }
    Ok(result)
}

fn prepare_custody(
    members: &Members,
    manifest: &Value,
    graph: &str,
    user: &str,
) -> Result<BTreeMap<String, String>, String> {
    let mut metadata = BTreeMap::new();
    if manifest["transformation"] == V21 {
        return Ok(metadata);
    }
    let custody = parse_json(member(members, "source-custody/index.json")?)?;
    require(
        custody["format"] == "cloud1-source-custody-v1"
            && custody["userId"] == user
            && custody["graphId"] == graph
            && custody["destinationAuthority"] == "not-conferred",
        "source custody identity/authority mismatch",
    )?;
    require_lower_sha256(
        string(&custody, "inventorySha256")?,
        "source inventory hash",
    )?;
    require(
        !string(&custody, "writerBoundary")?.is_empty(),
        "source writer boundary missing",
    )?;
    array(&custody, "dynamoRecords")?;
    array(&custody, "deleteMarkers")?;
    let mut paths = set(&["source-custody/index.json"]);
    let mut identities = BTreeSet::new();
    let null_policy = null_version_policy(members, manifest)?;
    let mut null_versions = 0u64;
    let mut null_buckets = BTreeSet::new();
    let expected_null_origins = object(manifest, "sourceObjects")?
        .values()
        .filter(|source| source.get("version_id").and_then(Value::as_str) == Some("null"))
        .map(|source| {
            json_bytes(&json!([
                string(source, "bucket")?,
                string(source, "key")?,
                "null",
                string(source, "sha256")?,
                number(source, "byte_length")?
            ]))
        })
        .collect::<Result<BTreeSet<_>, _>>()?;
    let mut actual_null_origins = BTreeSet::new();
    let mut catalogue_body = None;
    for entry in array(&custody, "objects")? {
        let version = string(entry, "version_id")?;
        let identity = json!([string(entry, "bucket")?, string(entry, "key")?, version]);
        require(
            !version.is_empty() && (version != "null" || null_policy.is_some()),
            "unversioned source custody object",
        )?;
        if version == "null" {
            null_versions += 1;
            null_buckets.insert(string(entry, "bucket")?.to_string());
            actual_null_origins.insert(json_bytes(&json!([
                string(entry, "bucket")?,
                string(entry, "key")?,
                "null",
                string(entry, "sha256")?,
                number(entry, "byte_length")?
            ]))?);
        }
        let key = archive_sha256(&json_bytes(&identity)?);
        let path = string(entry, "member")?;
        require(
            path == format!("source-custody/objects/{key}.bin")
                && paths.insert(path.to_string())
                && identities.insert(key),
            "duplicate or unbound source custody object",
        )?;
        let data = member(members, path)?;
        require(
            number(entry, "byteLength")? == data.len() as u64
                && number(entry, "byte_length")? == data.len() as u64
                && entry["sha256"] == archive_sha256(data)
                && entry["latest"].is_boolean()
                && entry["metadata"].is_object(),
            "source custody object metadata mismatch",
        )?;
        let catalogue = &custody["catalogueSource"];
        if ["bucket", "key", "version_id"]
            .iter()
            .all(|k| entry[*k] == catalogue[*k])
        {
            require(
                entry["sha256"] == catalogue["sha256"]
                    && entry["byte_length"] == catalogue["byte_length"]
                    && catalogue_body.replace(data).is_none(),
                "ambiguous catalogue source",
            )?;
        }
    }
    match null_policy {
        Some((selected, enabled_buckets)) => require(
            null_versions == selected
                && !null_buckets.is_empty()
                && null_buckets.is_subset(&enabled_buckets),
            "literal null custody count or bucket testimony mismatch",
        )?,
        None => require(null_versions == 0, "unversioned source custody object")?,
    }
    require(
        expected_null_origins.is_subset(&actual_null_origins),
        "literal null semantic/custody origin mismatch",
    )?;
    if manifest["transformation"] == V23 {
        require(
            custody["sourceCompleteness"] == legacy_completeness(),
            "legacy custody invented source completeness",
        )?;
        let held = array(&custody, "heldFiles")?;
        require(held.len() <= 1000, "held file count budget")?;
        let mut domains = json!({"snapshots":"captured-exact","rdf":"captured-exact","catalogue":"captured-exact",
            "redis":"not-captured","catalogueWal":"not-captured","processMemory":"not-captured","journalTails":"not-captured"});
        for entry in held {
            let kind = string(entry, "kind")?;
            require(
                matches!(kind, "redis-rdb" | "redis-aof" | "catalogue-wal"),
                "unsupported held source kind",
            )?;
            let label = string(entry, "sourceLabel")?;
            source_id(label)?;
            let path = string(entry, "member")?;
            require(
                path == format!("source-custody/held-files/{kind}/{label}.bin")
                    && paths.insert(path.to_string())
                    && entry["sourceScope"] == "retained-local-copy"
                    && entry["semanticRestoreAuthority"] == false,
                "held source path or authority mismatch",
            )?;
            let data = member(members, path)?;
            require(
                number(entry, "byteLength")? == data.len() as u64
                    && entry["sha256"] == archive_sha256(data),
                "held source byte mismatch",
            )?;
            domains[if kind == "catalogue-wal" {
                "catalogueWal"
            } else {
                "redis"
            }] = json!("captured-opaque-copy");
        }
        require(
            custody["legacyDomains"] == domains,
            "legacy source domain disposition mismatch",
        )?;
    } else {
        require(
            custody["heldFiles"].is_null() || custody["heldFiles"] == json!([]),
            "held source files require explicit v2.3",
        )?;
    }
    exact_class(manifest, "source-custody", paths)?;
    let slice = string(&custody, "graphMetadataNQuads")?;
    let catalogue = catalogue_body.ok_or("source graph catalogue byte custody missing")?;
    let mut decoder = flate2::bufread::GzDecoder::new(catalogue);
    let mut raw = Vec::new();
    decoder
        .by_ref()
        .take(MEMBER_LIMIT as u64 + 1)
        .read_to_end(&mut raw)
        .map_err(|e| e.to_string())?;
    require(
        raw.len() <= MEMBER_LIMIT && decoder.into_inner().is_empty(),
        "catalogue expansion/trailing member refused",
    )?;
    let source_subject = format!("urn:mnemosyne:user:{user}:graph:{graph}");
    let source_graph = format!("urn:mnemosyne:user:{user}:meta");
    let belongs = |quad: &oxigraph::model::Quad| {
        matches!(&quad.subject,NamedOrBlankNode::NamedNode(n) if n.as_str() == source_subject)
            && matches!(&quad.graph_name,GraphName::NamedNode(n) if n.as_str() == source_graph)
    };
    let mut actual = BTreeSet::new();
    for quad in RdfParser::from_format(RdfFormat::NQuads).for_slice(&raw) {
        let quad = quad.map_err(|e| e.to_string())?;
        if belongs(&quad) {
            actual.insert(quad.to_string());
        }
    }
    let mut selected = BTreeSet::new();
    for quad in RdfParser::from_format(RdfFormat::NQuads).for_slice(slice.as_bytes()) {
        let quad = quad.map_err(|e| e.to_string())?;
        require(
            belongs(&quad) && selected.insert(quad.to_string()),
            "foreign/duplicate graph metadata assertion",
        )?;
        let key = match quad.predicate.as_str() {
            "http://purl.org/dc/terms/title" => "title",
            "http://purl.org/dc/terms/description" => "description",
            _ => continue,
        };
        let Term::Literal(value) = &quad.object else {
            return Err("source graph metadata must be a literal".into());
        };
        require(
            value.language().is_none()
                && value.datatype().as_str() == "http://www.w3.org/2001/XMLSchema#string",
            "unsupported localized/typed graph display metadata",
        )?;
        require(
            metadata.insert(key.into(), value.value().into()).is_none(),
            "conflicting source graph metadata",
        )?;
    }
    require(
        selected == actual && !selected.is_empty(),
        "catalogue slice differs from exact source object",
    )?;
    if let Some(title) = metadata.get("title") {
        require(
            crate::ids::normalize_stored_title(title)? == *title,
            "source graph title would be normalized",
        )?;
    }
    Ok(metadata)
}

// Only fresh, importer-owned directories are created. Archive names are not
// native paths; their sole extraction root is this private source-custody tree.
fn mkdir_checked(root: &Path, relative: &Path) -> Result<PathBuf, String> {
    let mut current = root.to_path_buf();
    for component in relative.components() {
        let std::path::Component::Normal(name) = component else {
            return Err("nonlocal custody directory".into());
        };
        current.push(name);
        match fs::symlink_metadata(&current) {
            Ok(meta) => require(
                meta.is_dir() && !meta.file_type().is_symlink(),
                "custody parent is not a real directory",
            )?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                #[cfg(unix)]
                {
                    use std::os::unix::fs::DirBuilderExt;
                    fs::DirBuilder::new()
                        .mode(0o700)
                        .create(&current)
                        .map_err(|e| e.to_string())?;
                }
                #[cfg(not(unix))]
                fs::create_dir(&current).map_err(|e| e.to_string())?;
                crate::storage_atomic::sync_parent_dir(
                    current.parent().ok_or("custody parent missing")?,
                )?;
            }
            Err(e) => return Err(e.to_string()),
        }
    }
    Ok(current)
}
fn write_new(root: &Path, relative: &str, bytes: &[u8]) -> Result<(), String> {
    safe_path(relative)?;
    let path = Path::new(relative);
    let parent = mkdir_checked(root, path.parent().ok_or("custody file parent missing")?)?;
    let destination = parent.join(path.file_name().ok_or("custody filename missing")?);
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        // create_new is atomic and refuses an existing file OR symlink.
        options.mode(0o600);
    }
    let mut file = options.open(&destination).map_err(|e| {
        format!(
            "exclusive preservation write {}: {e}",
            destination.display()
        )
    })?;
    file.write_all(bytes)
        .and_then(|_| file.sync_all())
        .map_err(|e| e.to_string())?;
    crate::storage_atomic::sync_parent_dir(&parent)
}

fn persist_history(graph_dir: &Path, items: &[History]) -> Result<Vec<Value>, String> {
    let mut documents = BTreeMap::<&str, Vec<&History>>::new();
    let mut payloads = Vec::new();
    for item in items {
        documents
            .entry(&item.meta.document_id)
            .or_default()
            .push(item);
    }
    for (document, mut imported) in documents {
        let mut store = crate::document_history_file_store::read_document_history_store(
            graph_dir,
            &imported[0].meta.graph_id,
            document,
        )?;
        // Historical source events precede the newly materialized destination
        // commit. Preserve native append order: its last payload is the baseline
        // for the next edit, irrespective of skew in the source clock.
        imported
            .sort_by_key(|item| crate::document_history_persistence::history_sort_key(&item.meta));
        let mut snapshots = Vec::with_capacity(imported.len() + store.snapshots.len());
        for item in imported {
            require(
                !store
                    .snapshots
                    .iter()
                    .any(|s| s.snapshot_id == item.meta.snapshot_id),
                "source/native history identity collision",
            )?;
            let relative = format!(
                "documents/{document}/history/snapshots/{}.json",
                item.meta.snapshot_id
            );
            let bytes = json_bytes(&item.payload)?;
            write_new(graph_dir, &relative, &bytes)?;
            payloads.push(json!({"snapshotId":item.meta.snapshot_id,"documentId":document,
                "path":relative,"byteLength":bytes.len(),"sha256":archive_sha256(&bytes)}));
            snapshots.push(item.meta.clone());
            store.total_count = store
                .total_count
                .checked_add(item.meta.snapshot_count)
                .ok_or("history count overflow")?;
        }
        snapshots.extend(store.snapshots);
        store.snapshots = snapshots;
        crate::document_history_file_store::write_document_history_store(graph_dir, &store)?;
    }
    payloads.sort_by_key(|row| row["path"].as_str().unwrap_or("").to_string());
    Ok(payloads)
}

fn persist_graph_history(
    graph_dir: &Path,
    graph: &str,
    points: &[GraphHistory],
) -> Result<(), String> {
    use crate::time_travel_store;
    use crate::time_travel_types::RestorePointIndexEntry;
    let mut index = time_travel_store::read_index(graph_dir, graph)
        .map_err(crate::app_error::AppError::message)?;
    for point in points {
        let m = &point.manifest;
        require(
            !index
                .entries
                .iter()
                .any(|e| e.restore_point_id == m.restore_point_id),
            "restore point identity collision",
        )?;
        require(
            !crate::time_travel_paths::restore_point_dir(graph_dir, &m.restore_point_id).exists(),
            "restore point path occupied",
        )?;
        time_travel_store::write_workspace_bundle(
            graph_dir,
            &m.restore_point_id,
            &point.workspace,
            &point.snapshot,
        )
        .map_err(crate::app_error::AppError::message)?;
        for (id, bytes) in &point.documents {
            time_travel_store::write_document_bytes(graph_dir, &m.restore_point_id, id, bytes)
                .map_err(crate::app_error::AppError::message)?;
        }
        for payload in &point.payloads {
            write_new(
                graph_dir,
                &format!(
                    "documents/{}/history/snapshots/{}.json",
                    payload.document_id, payload.snapshot_id
                ),
                &json_bytes(&payload.native())?,
            )?;
        }
        time_travel_store::write_manifest(graph_dir, m)
            .map_err(crate::app_error::AppError::message)?;
        index.entries.push(RestorePointIndexEntry {
            restore_point_id: m.restore_point_id.clone(),
            created_at: m.created_at,
            trigger: m.trigger,
            label: m.label.clone(),
            content_hash_sha256: m.content_hash_sha256.clone(),
            size_bytes: m.metadata.workspace_size_bytes + m.metadata.total_document_size_bytes,
            document_count: m.metadata.document_count,
            folder_count: m.metadata.folder_count,
            artifact_count: m.metadata.artifact_count,
        });
    }
    if !points.is_empty() {
        time_travel_store::write_index(graph_dir, &index)
            .map_err(crate::app_error::AppError::message)?;
    }
    Ok(())
}

fn persist_original(graph_dir: &Path, original: &Original, members: &Members) -> Result<Value,String> {
    let destination=match original.kind.as_str() {
        "document"=>crate::paths::document_dir(graph_dir,&original.id)?.join("original"),
        "artifact"=>crate::paths::artifact_original_dir(graph_dir,&original.id)?,
        "image"=>crate::paths::image_original_dir(graph_dir,&original.id)?,
        _=>return Err("unreachable original kind".into()),
    };
    require(!destination.exists(),"original destination occupied")?;
    let source=member(members,&original.member)?;
    crate::original_file_storage::save_original_bytes_to_dir(&destination,&original.filename,&original.mime,source)?;
    if let Some(source_filename) = original.provenance.get("sourceFilename").and_then(Value::as_str) {
        if source_filename != original.filename {
            let mut manifest = crate::original_file_manifest_store::read_original_manifest(&destination)?;
            manifest.source_filename = Some(source_filename.to_string());
            crate::storage::write_json(&destination.join("manifest.json"), &manifest).map_err(|e| e.to_string())?;
        }
    }
    let (manifest,body)=crate::original_file_storage::read_original_file_from_dir(&destination)?;
    require(body==source && manifest.filename==original.filename && manifest.mime_type==original.mime,"persisted original readback differs from admitted bytes/metadata")?;
    let manifest_bytes=fs::read(destination.join("manifest.json")).map_err(|e|e.to_string())?;
    let relative=destination.strip_prefix(graph_dir).map_err(|e|e.to_string())?.to_string_lossy();
    Ok(json!({"provenance":original.provenance,
        "payload":{"path":format!("{relative}/{}",original.filename),"sha256":archive_sha256(&body),"byteLength":body.len()},
        "manifest":{"path":format!("{relative}/manifest.json"),"sha256":archive_sha256(&manifest_bytes),"byteLength":manifest_bytes.len()}}))
}

pub(super) fn apply(
    app: &AppHandle,
    operation: &CrdtOperation,
    bytes: &[u8],
    pending: Option<&str>,
    envelope_hash: String,
) -> ApplyOperationResult<Value> {
    let prepared = prepare(bytes, operation).map_err(ApplyOperationError::terminal)?;
    let graph = &operation.graph_id;
    let (graph_dir, before) = crate::graph_record_store::read_graph_record_no_heal(app, graph)
        .map_err(crate::app_error::AppError::message)?;
    let incarnation = before
        .incarnation_id
        .as_deref()
        .ok_or_else(|| ApplyOperationError::terminal("preservation target incarnation missing"))?;
    require(
        operation.payload["graphIncarnation"] == incarnation,
        "preservation incarnation mismatch",
    )?;
    // The executor holds the graph persistence/lifetime lease. Refuse harnesses
    // without the same managed admission machinery rather than claiming parity.
    require(
        app.try_state::<super::super::persistence_coordinator::GraphPersistenceCoordinator>()
            .is_some(),
        "managed persistence coordinator required",
    )?;
    let registry = app
        .try_state::<super::super::rooms::RoomRegistry>()
        .ok_or_else(|| ApplyOperationError::terminal("preservation room registry required"))?;
    let _rooms = registry
        .begin_disk_restore(graph)
        .map_err(ApplyOperationError::terminal)?;
    let custody = graph_dir.join(ROOT);
    if fs::symlink_metadata(&custody).is_ok() {
        return Err(ApplyOperationError::terminal("preservation v2: incomplete or occupied custody claim; replay cannot overwrite possible destination edits. Retain source custody and recover into a separately authorized empty generation"));
    }
    require_empty_cell_restore_target(app, &graph_dir, graph)
        .map_err(ApplyOperationError::terminal)?;
    for path in [
        graph_dir.join("restore-points"),
        graph_dir.join(".migration"),
        graph_dir.join("source-sync"),
    ] {
        require(
            !directory_has_visible_entries(&path)?,
            "pre-existing history, restore claim or source authority occupies target",
        )?;
    }
    let mut claim = json!({"schemaVersion":1,"operationId":operation.operation_id,"targetGraphId":graph,
        "targetGraphIncarnation":incarnation,"targetGeneration":operation.payload["targetGeneration"],
        "archiveSha256":archive_sha256(bytes),"envelopeSha256":envelope_hash,"planDigest":operation.payload["planDigest"],
        "sourceUserId":prepared.manifest["source"]["userId"],"sourceGraphId":graph,
        "transformation":prepared.manifest["transformation"],"startedAt":operation.enqueue_timestamp,
        "resumePolicy":"completed-ledger-only; incomplete-claim-refuses"});
    if !prepared.unavailable_bodies.is_empty() {
        claim["unavailableBodiesSha256"] = json!(archive_sha256(&json_bytes(&prepared.unavailable_bodies)?));
    }
    // An existing path (including a symlink or abandoned empty directory) is
    // never adopted. A storage error leaves inspectable owned partial custody.
    mkdir_checked(&graph_dir, Path::new(".migration"))?;
    fs::create_dir(&custody)
        .map_err(|e| ApplyOperationError::terminal(format!("claim preservation directory: {e}")))?;
    crate::storage_atomic::sync_parent_dir(&graph_dir.join(".migration"))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&custody, fs::Permissions::from_mode(0o700))
            .map_err(|e| e.to_string())?;
    }
    let perform = || -> Result<Value, String> {
        write_new(&custody, "claim.json", &json_bytes(&claim)?)?;
        write_new(&custody, "archive.tar.gz", bytes)?;
        for (path, body) in &prepared.members {
            write_new(&custody, &format!("source/{path}"), body)?;
        }
        let disposition = json!({"schemaVersion":1,"rdfNamedGraphs":prepared.graph_mapping,
            "retainedDatasetPartitions":prepared.dataset_partitions,
            "legacyProjectionNormalization":prepared.timestamp_normalization,
            "sourceDerivedAssertionDisposition":prepared.derived_disposition,
            "contentParityDisposition":prepared.content_parity_disposition,
            "rdfPreservationAccounting":prepared.rdf_accounting,
            "rawSourceAuthority":"not-conferred","destinationDeletionFence":"new native operation-bound identity/time; source null fields retained",
            "sourceGraphMetadata":prepared.metadata,"destinationCreatedAt":before.created_at,
            "documentHistoryUnknownRevision":null,"documentHistoryMissingTitleDisplay":"document ID",
            "unavailableDocumentHistory":prepared.unavailable_history,
            "documentHistoryAvailability":prepared.history_availability,
            "documentHistoryDisposition":prepared.history_disposition,
            "historyTimeAdapter":"RFC3339 to unsigned epoch milliseconds; original text and sub-ms precision retained in source custody; pre-epoch refused",
            "sourceCompleteness":prepared.manifest["source"]["capture"]["sourceCompleteness"],
            "graphHistoryTrigger":"imported checkpoint; source trigger unspecified",
            "graphHistoryContentHash":"SHA256 of retained source graph manifest; source-graph-* payload IDs are adapter references, not source events",
            "rawArchiveSha256":archive_sha256(bytes)});
        write_new(&custody, "disposition.json", &json_bytes(&disposition)?)?;
        let history_disposition_bytes=json_bytes(&prepared.history_disposition)?;
        write_new(&custody,"document-history.json",&history_disposition_bytes)?;
        let history_disposition_file=json!({"path":format!("{ROOT}/document-history.json"),
            "sha256":archive_sha256(&history_disposition_bytes),"byteLength":history_disposition_bytes.len()});
        write_new(
            &custody,
            "custody-complete.json",
            &json_bytes(
                &json!({"archiveSha256":archive_sha256(bytes),"members":prepared.members.len()}),
            )?,
        )?;
        if !prepared.unavailable_bodies.is_empty() {
            let mut availability = claim.clone();
            availability["state"] = json!("source-body-unavailable");
            availability["documents"] = json!(prepared.unavailable_bodies);
            write_new(&custody, "unavailable-bodies.json", &json_bytes(&availability)?)?;
        }
        let input: crate::document_types::SaveWorkspaceInput = serde_json::from_value(json!({"graphId":graph,
            "ydocUpdateBase64":encode_bytes_base64(&prepared.workspace),"snapshot":prepared.snapshot,"traceOperationId":operation.operation_id})).map_err(|e|e.to_string())?;
        crate::document_persistence_service::save_workspace_with_lease(app.clone(), input)?;
        #[cfg(test)]
        maybe_fail_archive_step_for_test(
            &operation.operation_id,
            ArchiveFailurePoint::BeforeDocument,
        )
        .map_err(ApplyOperationError::into_message)?;
        for (id, body) in &prepared.parsed.documents {
            let title = prepared.snapshot["documents"]
                .as_array()
                .and_then(|rows| rows.iter().find(|r| r["id"] == *id))
                .and_then(|r| r["title"].as_str())
                .map(str::to_string);
            import_one_document(app, graph, id, body, title, &operation.operation_id)?;
        }
        let original_payloads=prepared.originals.iter().map(|original|persist_original(&graph_dir,original,&prepared.members)).collect::<Result<Vec<_>,_>>()?;
        let history_payloads = persist_history(&graph_dir, &prepared.history)?;
        persist_graph_history(&graph_dir, graph, &prepared.graph_history)?;
        let mut native_deletions = Vec::new();
        for id in &prepared.deletions {
            let fence = crate::document_tombstone_store::write_document_tombstone_for_operation(
                &graph_dir,
                id,
                Some(&operation.operation_id),
            )?;
            native_deletions.push(serde_json::to_value(fence).map_err(|e| e.to_string())?);
        }
        crate::rdf_service::load_rdf_dataset(
            app.clone(),
            crate::rdf_service::RdfLoadInput {
                graph_id: graph.clone(),
                data: prepared.rdf.clone(),
                format: "application/n-quads".into(),
                base_iri: None,
                target_graph_iri: None,
            },
        )?;
        if !prepared.metadata.is_empty() {
            // Already under the executor's graph lease. Preserve target
            // lifecycle fields and source literal whitespace; no metadata
            // service normalization and no foreign catalog/ACL import.
            let updated = crate::graph_record_store::mutate_graph_record(&graph_dir, |record| {
                if record.incarnation_id.as_deref() != Some(incarnation) {
                    return Err(crate::app_error::AppError::validation(
                        "target incarnation changed",
                    ));
                }
                if let Some(title) = prepared.metadata.get("title") {
                    record.title = title.clone();
                }
                if let Some(description) = prepared.metadata.get("description") {
                    record.description = Some(description.clone());
                }
                let revision = crate::graph_record_store::next_content_revision(
                    record.content_revision.as_deref(),
                )?;
                record.updated_at = revision.clone();
                record.content_revision = Some(revision);
                Ok(record.clone())
            })
            .map_err(crate::app_error::AppError::message)?;
            let store = crate::rdf_store_service::open_graph_store(&graph_dir)?;
            crate::rdf_record_materializer::reconcile_graph_record(&store, &updated)?;
            crate::graph_service::repair_published_graph_projections_best_effort(app, &updated);
        }
        write_new(
            &custody,
            "native-deletion-fences.json",
            &json_bytes(&native_deletions)?,
        )?;
        let (_, after) = crate::graph_record_store::read_graph_record_no_heal(app, graph)
            .map_err(crate::app_error::AppError::message)?;
        require(
            after.incarnation_id == before.incarnation_id && after.created_at == before.created_at,
            "target lifecycle changed",
        )?;
        let result = json!({"type":"import_graph","formatVersion":2,"restoredExistingGraph":true,"graphId":graph,"graph_id":graph,
            "documentCount":prepared.parsed.documents.len(),"document_count":prepared.parsed.documents.len(),
            "documentIds":prepared.parsed.documents.iter().map(|(id,_)|id).collect::<Vec<_>>(),
            "rdfTripleCount":prepared.manifest["counts"]["rdfQuads"],"workspaceImported":true,"includesArtifacts":true,
            "sourceUserId":prepared.manifest["source"]["userId"],"sourceGraphId":graph,"transformation":prepared.manifest["transformation"],
            "counts":prepared.manifest["counts"],"regeneratedRdfStatementCount":prepared.regenerated,"rdfNamedGraphs":prepared.graph_mapping,
            "retainedDatasetPartitions":prepared.dataset_partitions,
            "unavailableBodies":prepared.unavailable_bodies,"presentDocumentBodyCount":prepared.parsed.documents.len(),
            "metadataDocumentCount":prepared.manifest["counts"]["documents"],
            "legacyProjectionNormalization":prepared.timestamp_normalization,
            "sourceDerivedAssertionDisposition":prepared.derived_disposition,
            "contentParityDisposition":prepared.content_parity_disposition,
            "retainedDerivedRdfStatementCount":prepared.derived_disposition["entries"].as_array().map_or(0,Vec::len),
            "rdfPreservationAccounting":prepared.rdf_accounting,
            "rdfMappedTestimonyFullSet":prepared.rdf_accounting["rdfMappedTestimonyFullSet"],
            "originalPayloads":original_payloads,
            "documentHistoryPayloads":history_payloads,
            "unavailableDocumentHistory":prepared.unavailable_history,
            "documentHistoryAvailability":prepared.history_availability,
            "documentHistoryDisposition":prepared.history_disposition,
            "documentHistoryDispositionFile":history_disposition_file,
            "documentHistoryInterpretation": if prepared.manifest["transformation"] == V23 { json!({
                "schema":"cloud1-exported-history-interpretation.v1", "rawSourceRetained":true,
                "originalHistoricalCrdtReconstructed":false, "recognizedBalancedInlineTags":"interpreted-as-marks",
                "structuralInlineAtoms":"exact-empty-wikilink-tagChip-footnote-frames-with-source-text-and-native-attribute-equality",
                "otherAngleSyntax":"literal", "textAmpersands":"literal", "codeBlockText":"literal"}) } else { Value::Null },
            "graphHistory":prepared.graph_history.iter().map(graph_history_disposition).collect::<Vec<_>>(),
            "archiveSha256":archive_sha256(bytes),"planDigest":operation.payload["planDigest"],"targetGeneration":operation.payload["targetGeneration"],
            "graph":after,"sourceCustodyPath":ROOT,"sourceCutAuthentication":"not independently attested by importer",
            "sourceCompleteness":prepared.manifest["source"]["capture"]["sourceCompleteness"],
            "sourceGraphMetadataSupplied":!prepared.metadata.is_empty(),"resumePolicy":"completed-ledger-only; incomplete-claim-refuses"});
        write_new(
            &custody,
            "materialization-complete.json",
            &json_bytes(&result)?,
        )?;
        Ok(result)
    };
    let result = perform().map_err(ApplyOperationError::retryable_after_hot_commit)?;
    crate::operation_completion_ledger::append_completion_entry(
        app,
        crate::operation_completion_ledger::OperationCompletionEntry {
            schema_version: 1,
            operation_id: operation.operation_id.clone(),
            kind: operation.kind.clone(),
            graph_id: Some(graph.clone()),
            completed_at: operation.enqueue_timestamp.clone(),
            payload_hash: Some(envelope_hash),
            result: Some(result.clone()),
        },
    )
    .map_err(ApplyOperationError::retryable_after_hot_commit)?;
    if let Some(path) = pending {
        cleanup_pending_archive(app, path, Some(graph));
    }
    Ok(result)
}

//! Owner-only reads of immutable legacy evidence. Never a native revision store.
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::{
    fs,
    io::{Read, Write},
    path::{Component, Path},
};

const ROOT: &str = ".migration/preservation-v2";
const MAX_BYTES: u64 = 512 * 1024 * 1024;
const RESPONSE_BYTES: usize = 8 * 1024 * 1024;

struct BoundedOutput(Vec<u8>);
impl Write for BoundedOutput {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        if self
            .0
            .len()
            .checked_add(bytes.len())
            .is_none_or(|size| size > RESPONSE_BYTES)
        {
            return Err(std::io::Error::other(
                "legacy response byte budget exceeded",
            ));
        }
        self.0.extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

pub(crate) fn metadata_bytes(value: &Value) -> Result<Vec<u8>, String> {
    let mut output = BoundedOutput(Vec::new());
    serde_json::to_writer(&mut output, value).map_err(|e| e.to_string())?;
    Ok(output.0)
}
fn hash(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn read(root: &Path, relative: &str) -> Result<Vec<u8>, String> {
    let mut ancestor = Some(root);
    while let Some(path) = ancestor {
        let meta = fs::symlink_metadata(path).map_err(|_| "legacy history not found")?;
        if meta.file_type().is_symlink() {
            return Err("legacy history symlink refused".into());
        }
        ancestor = path.parent();
    }
    let mut path = root.to_path_buf();
    for part in Path::new(relative).components() {
        let Component::Normal(name) = part else {
            return Err("nonlocal legacy history path".into());
        };
        path.push(name);
        let meta = fs::symlink_metadata(&path).map_err(|_| "legacy history not found")?;
        if meta.file_type().is_symlink() {
            return Err("legacy history symlink refused".into());
        }
    }
    let file = fs::File::open(path).map_err(|e| e.to_string())?;
    let meta = file.metadata().map_err(|e| e.to_string())?;
    if !meta.is_file() || meta.len() > MAX_BYTES {
        return Err("legacy history file budget/type refused".into());
    }
    let mut bytes = Vec::new();
    file.take(MAX_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| e.to_string())?;
    if bytes.len() as u64 > MAX_BYTES {
        return Err("legacy history read budget exceeded".into());
    }
    Ok(bytes)
}

pub(crate) fn catalog(graph_dir: &Path, graph: &str, owner: &str) -> Result<Value, String> {
    let root = graph_dir.join(ROOT);
    let completion: Value = serde_json::from_slice(&read(&root, "materialization-complete.json")?)
        .map_err(|e| e.to_string())?;
    let bytes = read(&root, "document-history.json")?;
    let value: Value = serde_json::from_slice(&bytes).map_err(|e| e.to_string())?;
    if value["schema"] != "cloud1-document-history-disposition.v1"
        || value["sourceGraphId"] != graph
        || value["sourceUserId"] != owner
        || completion["sourceGraphId"] != graph
        || completion["sourceUserId"] != owner
        || completion["documentHistoryDisposition"] != value
        || completion["documentHistoryDispositionFile"]["sha256"] != hash(&bytes)
        || completion["documentHistoryDispositionFile"]["byteLength"] != bytes.len()
        || completion["documentHistoryDispositionFile"]["path"]
            != format!("{ROOT}/document-history.json")
    {
        return Err("legacy history disposition binding mismatch".into());
    }
    let rows = value["entries"]
        .as_array()
        .ok_or("legacy history entries missing")?;
    let mut ids = std::collections::BTreeSet::new();
    let mut counts = [0usize; 3];
    for row in rows {
        if row["sourceUserId"] != owner
            || row["sourceGraphId"] != graph
            || !ids.insert(
                row["snapshotId"]
                    .as_str()
                    .ok_or("legacy snapshot ID missing")?,
            )
            || !matches!(
                row["status"].as_str(),
                Some("native-interpreted" | "legacy-read-only" | "source-payload-unavailable")
            )
        {
            return Err("legacy history entry scope/status mismatch".into());
        }
        let native = row["status"] == "native-interpreted";
        if row["nativeSnapshotCreated"] != native
            || row["nativeRestorable"] != native
            || row["readOnly"] != !native
            || row["restoreSupported"] != native
        {
            return Err("legacy history native authority mismatch".into());
        }
        counts[if native {
            0
        } else if row["status"] == "legacy-read-only" {
            1
        } else {
            2
        }] += 1;
        if row["status"] == "source-payload-unavailable" && row.get("sourceMember").is_some() {
            return Err("unavailable history claims source payload".into());
        }
    }
    if value["retainedMetadataCount"] != rows.len()
        || value["nativeInterpretedCount"] != counts[0]
        || value["legacyReadOnlyCount"] != counts[1]
        || value["unavailablePayloadCount"] != counts[2]
    {
        return Err("legacy history partition count mismatch".into());
    }
    Ok(value)
}

pub(crate) fn entry(catalog: &Value, document: &str, snapshot: &str) -> Result<Value, String> {
    catalog["entries"]
        .as_array()
        .ok_or("legacy history entries missing")?
        .iter()
        .find(|row| row["documentId"] == document && row["snapshotId"] == snapshot)
        .cloned()
        .ok_or_else(|| "legacy history not found".into())
}

pub(crate) fn source_bytes(graph_dir: &Path, entry: &Value) -> Result<Vec<u8>, String> {
    if entry["status"] == "source-payload-unavailable" {
        return Err("source-payload-unavailable".into());
    }
    let member = entry["sourceMember"]
        .as_str()
        .ok_or("legacy history source member missing")?;
    if !member.starts_with("history/documents/") || member == "history/documents/index.json" {
        return Err("legacy history source member refused".into());
    }
    let root = graph_dir.join(ROOT).join("source");
    let index_bytes = read(&root, "history/documents/index.json")?;
    let index: Value = serde_json::from_slice(&index_bytes).map_err(|e| e.to_string())?;
    if entry["sourceIndexMember"] != "history/documents/index.json"
        || entry["sourceIndexSha256"] != hash(&index_bytes)
        || !index
            .as_array()
            .ok_or("legacy source index invalid")?
            .contains(&entry["sourceMetadata"])
        || entry["sourceMetadata"]["owner_user_id"] != entry["sourceUserId"]
        || entry["sourceMetadata"]["graph_id"] != entry["sourceGraphId"]
        || entry["sourceMetadata"]["doc_id"] != entry["documentId"]
        || entry["sourceMetadata"]["snapshot_id"] != entry["snapshotId"]
        || entry["sourceMetadata"]["member"] != member
    {
        return Err("legacy history source index binding mismatch".into());
    }
    let bytes = read(&root, member)?;
    if entry["sourceMemberSha256"] != hash(&bytes) || entry["sourceByteLength"] != bytes.len() {
        return Err("legacy history source byte mismatch".into());
    }
    let body: Value = serde_json::from_slice(&bytes).map_err(|e| e.to_string())?;
    if body["snapshot_id"] != entry["snapshotId"]
        || body["created_at"] != entry["sourceMetadata"]["created_at"]
    {
        return Err("legacy history source identity mismatch".into());
    }
    Ok(bytes)
}

/// Literal, labelled source witness, not a parser or a native text projection.
pub(crate) fn literal_text(bytes: &[u8]) -> Result<String, String> {
    let body: Value = serde_json::from_slice(bytes).map_err(|e| e.to_string())?;
    let mut output = BoundedOutput(Vec::new());
    output.write_all("READ-ONLY LEGACY HISTORY — not a native revision; restore unsupported.\n\nSource XML (literal, not rendered):\n".as_bytes()).map_err(|e|e.to_string())?;
    output
        .write_all(
            body["tiptap_xml"]
                .as_str()
                .unwrap_or("[source XML field absent]")
                .as_bytes(),
        )
        .map_err(|e| e.to_string())?;
    output
        .write_all(b"\n\nSource blocks (literal JSON):\n")
        .map_err(|e| e.to_string())?;
    serde_json::to_writer(&mut output, body.get("blocks").unwrap_or(&Value::Null))
        .map_err(|e| e.to_string())?;
    String::from_utf8(output.0).map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    #[test]
    fn migration_legacy_literal_source_is_inert_and_labelled() {
        let bytes=br#"{"tiptap_xml":"<script>alert(1)</script><img onerror='bad' />","blocks":[{"text":"<a href='javascript:bad'>literal</a>"}]}"#;
        let text = super::literal_text(bytes).unwrap();
        assert!(text.starts_with("READ-ONLY LEGACY HISTORY"));
        assert!(text.contains("<script>alert(1)</script>"));
        assert!(text.contains("javascript:bad"));
        assert!(super::literal_text(b"not JSON").is_err());
    }
    #[test]
    fn migration_legacy_serialization_is_bounded_before_expansion() {
        use std::io::Write;
        let mut writer = super::BoundedOutput(Vec::new());
        assert!(writer
            .write_all(&vec![0; super::RESPONSE_BYTES + 1])
            .is_err());
        assert!(writer.0.is_empty());
        let body = serde_json::json!({"tiptap_xml":"x".repeat(super::RESPONSE_BYTES),"blocks":[]});
        let bytes = serde_json::to_vec(&body).unwrap();
        assert!(super::literal_text(&bytes).unwrap_err().contains("budget"));
        assert!(super::metadata_bytes(&body).unwrap_err().contains("budget"));
    }
}

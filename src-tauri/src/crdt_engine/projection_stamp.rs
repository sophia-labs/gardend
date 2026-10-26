//! "This cold document's record is the projection of that sidecar."
//!
//! The graph-wide read fence must prove every document's cold projection has
//! reached its Y.Doc authority. For a document with no live room the only way
//! to know that used to be to open it: decode the whole update history into a
//! room (which starts dirty by design), re-materialize it, compare, and evict
//! it again, on every query. A stamp makes the proof a few stats.
//!
//! When a projection completes, the flush writes `history/projection-stamp.json`
//! beside the document's tail marker. It names the identity (length, mtime,
//! inode) of the Y.Doc sidecar the projection was materialized from, captured
//! under the room's authority lock (so the file is exactly the state that was
//! projected), and the identity of the `document.json` record the projection
//! produced. A cold document is current when both files still have those
//! identities. Every way the sidecar or the record can move on (a room write,
//! `save_document`, restore, source pull, duplicate, a hand edit) replaces the
//! file and changes at least the mtime, so a stale stamp can only fail to match;
//! a failure to match means "take the slow path", which is the old behaviour.
//!
//! Racy timestamps. Two different contents can share a length and a coarse
//! mtime tick. A stamp taken while the sidecar was modified less than
//! [`RACY_WINDOW_MS`] ago therefore also carries a SHA-256 of the sidecar bytes,
//! and is trusted only while the bytes still hash to it (git's "racily clean"
//! rule). Such a stamp is rewritten without the digest once the file is old
//! enough. Stamps are advisory: they are written without fsync, and a missing,
//! torn or unreadable stamp is simply "not current".

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

const STAMP_FILE: &str = "projection-stamp.json";
const STAMP_SCHEMA_VERSION: u32 = 1;
/// A file modified this recently cannot be told apart from a rewrite in the
/// same filesystem tick by (length, mtime, inode) alone.
const RACY_WINDOW_MS: u64 = 2_000;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct FileIdent {
    len: u64,
    mtime_ns: u64,
    ino: u64,
}

impl FileIdent {
    fn mtime_ms(&self) -> u64 {
        self.mtime_ns / 1_000_000
    }
}

pub(crate) fn file_ident(path: &Path) -> Option<FileIdent> {
    let metadata = std::fs::metadata(path).ok()?;
    if !metadata.is_file() {
        return None;
    }
    let mtime_ns = metadata
        .modified()
        .ok()?
        .duration_since(UNIX_EPOCH)
        .ok()?
        .as_nanos() as u64;
    #[cfg(unix)]
    let ino = std::os::unix::fs::MetadataExt::ino(&metadata);
    #[cfg(not(unix))]
    let ino = 0;
    Some(FileIdent {
        len: metadata.len(),
        mtime_ns,
        ino,
    })
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

fn digest_of(path: &Path) -> Option<String> {
    let bytes = std::fs::read(path).ok()?;
    Some(format!("{:x}", Sha256::digest(&bytes)))
}

/// What a room saw of its own sidecar while it held the authority lock.
#[derive(Clone, Debug)]
pub(crate) struct SidecarWitness {
    ident: FileIdent,
    /// Present only when the file was modified inside the racy window.
    sha256: Option<String>,
}

/// Observe the sidecar. Call it where the doc state being projected is known
/// to equal the file (inside `Room::with_doc_version`, which holds the room's
/// authority lock, the lock a room write holds across its persist).
pub(crate) fn witness_sidecar(path: &Path) -> Option<SidecarWitness> {
    let ident = file_ident(path)?;
    let racy = now_ms().saturating_sub(ident.mtime_ms()) < RACY_WINDOW_MS;
    let sha256 = if racy { Some(digest_of(path)?) } else { None };
    // The file must not have moved while it was hashed.
    if file_ident(path)? != ident {
        return None;
    }
    Some(SidecarWitness { ident, sha256 })
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ProjectionStamp {
    schema_version: u32,
    sidecar: FileIdent,
    record: FileIdent,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    sidecar_sha256: Option<String>,
    stamped_at_ms: u64,
}

fn stamp_path(graph_dir: &Path, document_id: &str) -> PathBuf {
    crate::document_history_store::document_history_dir(graph_dir, document_id).join(STAMP_FILE)
}

fn record_path(graph_dir: &Path, document_id: &str) -> PathBuf {
    crate::paths::documents_dir(graph_dir)
        .join(document_id)
        .join("document.json")
}

fn sidecar_path(graph_dir: &Path, document_id: &str) -> PathBuf {
    crate::ydoc_paths::document_ydoc_state_path(graph_dir, document_id)
}

fn write_stamp_file(path: &Path, stamp: &ProjectionStamp) {
    let Ok(bytes) = serde_json::to_vec(stamp) else {
        return;
    };
    if let Some(parent) = path.parent() {
        if std::fs::create_dir_all(parent).is_err() {
            return;
        }
    }
    let temp = crate::storage_atomic::atomic_temp_path(path);
    // No fsync: a stamp that does not survive a crash is just "not current".
    if std::fs::write(&temp, bytes).is_err() || std::fs::rename(&temp, path).is_err() {
        let _ = std::fs::remove_file(&temp);
    }
}

/// Record that the document's record and tail now project `witness`. Call it
/// after the projection (record, RDF, history tail) completed for the state
/// the witness was taken from, holding the room's projection gate.
pub(crate) fn write_stamp(graph_dir: &Path, document_id: &str, witness: &SidecarWitness) {
    let Some(record) = file_ident(&record_path(graph_dir, document_id)) else {
        return;
    };
    write_stamp_file(
        &stamp_path(graph_dir, document_id),
        &ProjectionStamp {
            schema_version: STAMP_SCHEMA_VERSION,
            sidecar: witness.ident.clone(),
            record,
            sidecar_sha256: witness.sha256.clone(),
            stamped_at_ms: now_ms(),
        },
    );
}

/// Test hook: forget a document's stamp, as a graph that predates stamps has none.
#[cfg(test)]
pub(crate) fn forget_stamp_for_test(graph_dir: &Path, document_id: &str) {
    let _ = std::fs::remove_file(stamp_path(graph_dir, document_id));
}

/// True when the document's cold projection provably reflects its current
/// sidecar, decided from a small file and two stats (plus one read and hash
/// for a stamp taken inside the racy window). Never decodes the Y.Doc.
pub(crate) fn cold_projection_is_current(graph_dir: &Path, document_id: &str) -> bool {
    let stamp_file = stamp_path(graph_dir, document_id);
    let Ok(bytes) = std::fs::read(&stamp_file) else {
        return false;
    };
    let Ok(stamp) = serde_json::from_slice::<ProjectionStamp>(&bytes) else {
        return false;
    };
    if stamp.schema_version != STAMP_SCHEMA_VERSION {
        return false;
    }
    let sidecar = sidecar_path(graph_dir, document_id);
    if file_ident(&sidecar).as_ref() != Some(&stamp.sidecar)
        || file_ident(&record_path(graph_dir, document_id)).as_ref() != Some(&stamp.record)
    {
        return false;
    }
    let Some(expected) = stamp.sidecar_sha256.as_deref() else {
        return true;
    };
    if digest_of(&sidecar).as_deref() != Some(expected) {
        return false;
    }
    // The bytes are the projected bytes. Once the file is old enough that no
    // same-tick rewrite can still be pending, drop the digest so the next read
    // is a stat.
    if now_ms().saturating_sub(stamp.sidecar.mtime_ms()) >= RACY_WINDOW_MS
        && file_ident(&sidecar).as_ref() == Some(&stamp.sidecar)
    {
        write_stamp_file(
            &stamp_file,
            &ProjectionStamp {
                schema_version: STAMP_SCHEMA_VERSION,
                sidecar: stamp.sidecar,
                record: stamp.record,
                sidecar_sha256: None,
                stamped_at_ms: now_ms(),
            },
        );
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use uuid::Uuid;

    fn graph_dir_with_document(id: &str, sidecar: &[u8], record: &[u8]) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("garden-stamp-{}", Uuid::new_v4()));
        let sidecar_file = sidecar_path(&dir, id);
        std::fs::create_dir_all(sidecar_file.parent().unwrap()).unwrap();
        std::fs::write(&sidecar_file, sidecar).unwrap();
        let record_file = record_path(&dir, id);
        std::fs::create_dir_all(record_file.parent().unwrap()).unwrap();
        std::fs::write(&record_file, record).unwrap();
        dir
    }

    fn age(path: &Path, seconds: u64) {
        let old = SystemTime::now() - std::time::Duration::from_secs(seconds);
        let file = std::fs::OpenOptions::new().write(true).open(path).unwrap();
        file.set_modified(old).unwrap();
    }

    #[test]
    fn a_stamp_is_current_until_the_sidecar_or_the_record_moves() {
        let dir = graph_dir_with_document("d", b"state-one", b"{}");
        age(&sidecar_path(&dir, "d"), 60);
        assert!(!cold_projection_is_current(&dir, "d"), "no stamp yet");

        let witness = witness_sidecar(&sidecar_path(&dir, "d")).unwrap();
        assert!(witness.sha256.is_none(), "an old file needs no digest");
        write_stamp(&dir, "d", &witness);
        assert!(cold_projection_is_current(&dir, "d"));

        // A different sidecar of the SAME length: only the timestamp differs.
        std::fs::write(sidecar_path(&dir, "d"), b"state-two").unwrap();
        age(&sidecar_path(&dir, "d"), 30);
        assert!(!cold_projection_is_current(&dir, "d"), "sidecar moved");

        let witness = witness_sidecar(&sidecar_path(&dir, "d")).unwrap();
        write_stamp(&dir, "d", &witness);
        assert!(cold_projection_is_current(&dir, "d"));
        std::fs::write(record_path(&dir, "d"), b"{\"x\":1}").unwrap();
        assert!(!cold_projection_is_current(&dir, "d"), "record moved");
    }

    #[test]
    fn a_racy_stamp_is_trusted_only_while_the_bytes_hash_the_same() {
        let dir = graph_dir_with_document("d", b"state-one", b"{}");
        // Freshly written: inside the racy window, so the stamp carries a digest.
        let witness = witness_sidecar(&sidecar_path(&dir, "d")).unwrap();
        assert!(witness.sha256.is_some());
        write_stamp(&dir, "d", &witness);
        assert!(cold_projection_is_current(&dir, "d"));

        // Same length, same mtime and inode, different bytes: the case the
        // identity alone cannot see. The digest must refuse it.
        let sidecar = sidecar_path(&dir, "d");
        let before = file_ident(&sidecar).unwrap();
        let mtime = std::fs::metadata(&sidecar).unwrap().modified().unwrap();
        let file = std::fs::OpenOptions::new()
            .write(true)
            .open(&sidecar)
            .unwrap();
        std::io::Write::write_all(&mut &file, b"state-ONE").unwrap();
        file.set_modified(mtime).unwrap();
        assert_eq!(
            file_ident(&sidecar).unwrap(),
            before,
            "identity is blind to it"
        );
        assert!(!cold_projection_is_current(&dir, "d"), "the digest is not");
    }

    #[test]
    fn a_torn_or_foreign_stamp_is_not_current() {
        let dir = graph_dir_with_document("d", b"state", b"{}");
        age(&sidecar_path(&dir, "d"), 60);
        let witness = witness_sidecar(&sidecar_path(&dir, "d")).unwrap();
        write_stamp(&dir, "d", &witness);
        std::fs::write(stamp_path(&dir, "d"), b"{\"schemaVersion\":1,").unwrap();
        assert!(!cold_projection_is_current(&dir, "d"));
        std::fs::write(stamp_path(&dir, "d"), b"not json").unwrap();
        assert!(!cold_projection_is_current(&dir, "d"));
    }
}

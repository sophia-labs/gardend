use crate::{ids::validate_local_id, paths::document_dir};
use std::{
    fs::{self, OpenOptions},
    io::{ErrorKind, Write},
    path::{Path, PathBuf},
};
use uuid::Uuid;

pub(crate) const DOCUMENT_INCARNATION_HEADER: &str = "x-document-incarnation";
pub(crate) const DOCUMENT_INCARNATION_QUERY: &str = "document_incarnation";
const DOCUMENT_INCARNATION_FILE: &str = ".incarnation-id";

fn incarnation_path(graph_dir: &Path, document_id: &str) -> Result<PathBuf, String> {
    validate_local_id(document_id, "document_id")?;
    Ok(document_dir(graph_dir, document_id)?.join(DOCUMENT_INCARNATION_FILE))
}

fn parse_incarnation(path: &Path, value: &str) -> Result<String, String> {
    let normalized = value.trim();
    Uuid::parse_str(normalized).map_err(|error| {
        format!(
            "invalid document incarnation at {}: {error}",
            path.display()
        )
    })?;
    Ok(normalized.to_string())
}

/// The classified failure mode of [`ensure_document_incarnation_id_with_requested`]
/// (D21). `Mismatch` is the ONE case a caller should code
/// `stale_document_incarnation` — a genuine cross-writer identity race.
/// `Fault` is everything else (an invalid id, a corrupt on-disk sidecar UUID,
/// the document directory disappearing, read/write/sync I/O failure): a
/// local durability fault, never a remote lifecycle fact, and must never be
/// coded as a Law VI fence.
#[derive(Debug)]
pub(crate) enum DocumentIncarnationFault {
    Mismatch { expected: String, actual: String },
    Fault(String),
}

impl std::fmt::Display for DocumentIncarnationFault {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DocumentIncarnationFault::Mismatch { expected, actual } => write!(
                formatter,
                "document incarnation conflict: expected {expected}, actual {actual}"
            ),
            DocumentIncarnationFault::Fault(message) => formatter.write_str(message),
        }
    }
}

/// Return the durable identity of one document incarnation.
///
/// The sidecar lives inside the document directory, so the canonical deletion
/// tail removes it with the old document and a trusted same-ID recreation gets
/// a new UUID. `create_new` makes lazy migration of legacy documents safe under
/// concurrent snapshot/room reads: exactly one caller publishes the identity
/// and every loser rereads that winner.
pub(crate) fn ensure_document_incarnation_id(
    graph_dir: &Path,
    document_id: &str,
) -> Result<String, String> {
    ensure_document_incarnation_id_with_requested(graph_dir, document_id, None)
        .map_err(|fault| fault.to_string())
}

/// Publish or verify a caller-minted document-lifetime identity.
///
/// Offline document creation must be able to bind the provisional local
/// Y.Doc to the exact lifetime the cell will later create. `create_new` keeps
/// concurrent first writers safe; an already-published different identity is
/// a hard fence rather than a last-writer-wins replacement.
pub(crate) fn ensure_document_incarnation_id_with_requested(
    graph_dir: &Path,
    document_id: &str,
    requested: Option<&str>,
) -> Result<String, DocumentIncarnationFault> {
    let path = incarnation_path(graph_dir, document_id).map_err(DocumentIncarnationFault::Fault)?;
    let requested = requested
        .map(|value| parse_incarnation(&path, value))
        .transpose()
        .map_err(DocumentIncarnationFault::Fault)?;
    loop {
        match fs::read_to_string(&path) {
            Ok(value) => {
                let actual =
                    parse_incarnation(&path, &value).map_err(DocumentIncarnationFault::Fault)?;
                if requested
                    .as_deref()
                    .is_some_and(|expected| expected != actual)
                {
                    return Err(DocumentIncarnationFault::Mismatch {
                        expected: requested.as_deref().unwrap_or_default().to_string(),
                        actual,
                    });
                }
                return Ok(actual);
            }
            Err(error) if error.kind() == ErrorKind::NotFound => {}
            Err(error) => {
                return Err(DocumentIncarnationFault::Fault(format!(
                    "read document incarnation {}: {error}",
                    path.display()
                )))
            }
        }

        let parent = path.parent().ok_or_else(|| {
            DocumentIncarnationFault::Fault(format!(
                "document incarnation has no parent: {}",
                path.display()
            ))
        })?;
        // Never create the document directory here. The caller has already
        // established authoritative document existence. If deletion removes
        // the directory between that check and this write, fail closed instead
        // of recreating an empty directory whose token could later be inherited
        // by a same-ID replacement.
        if !parent.is_dir() {
            return Err(DocumentIncarnationFault::Fault(format!(
                "document directory disappeared before incarnation write: {}",
                parent.display()
            )));
        }
        let incarnation = requested
            .clone()
            .unwrap_or_else(|| Uuid::new_v4().to_string());
        match OpenOptions::new().write(true).create_new(true).open(&path) {
            Ok(mut file) => {
                file.write_all(incarnation.as_bytes()).map_err(|error| {
                    DocumentIncarnationFault::Fault(format!(
                        "write document incarnation {}: {error}",
                        path.display()
                    ))
                })?;
                file.sync_all().map_err(|error| {
                    DocumentIncarnationFault::Fault(format!(
                        "sync document incarnation {}: {error}",
                        path.display()
                    ))
                })?;
                return Ok(incarnation);
            }
            Err(error) if error.kind() == ErrorKind::AlreadyExists => continue,
            Err(error) => {
                return Err(DocumentIncarnationFault::Fault(format!(
                    "create document incarnation {}: {error}",
                    path.display()
                )))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identity_is_stable_until_the_document_directory_is_replaced() {
        let root =
            std::env::temp_dir().join(format!("sophia-document-incarnation-{}", Uuid::new_v4()));
        let graph_dir = root.join("graph-a");
        let document_id = "doc-a";

        fs::create_dir_all(document_dir(&graph_dir, document_id).unwrap()).unwrap();
        let first = ensure_document_incarnation_id(&graph_dir, document_id).unwrap();
        let again = ensure_document_incarnation_id(&graph_dir, document_id).unwrap();
        assert_eq!(first, again);

        fs::remove_dir_all(document_dir(&graph_dir, document_id).unwrap()).unwrap();
        fs::create_dir_all(document_dir(&graph_dir, document_id).unwrap()).unwrap();
        let replacement = ensure_document_incarnation_id(&graph_dir, document_id).unwrap();
        assert_ne!(first, replacement);

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn missing_document_directory_is_not_recreated() {
        let root =
            std::env::temp_dir().join(format!("sophia-document-incarnation-{}", Uuid::new_v4()));
        let graph_dir = root.join("graph-a");
        let document_id = "deleted-doc";
        let document_path = document_dir(&graph_dir, document_id).unwrap();

        let error = ensure_document_incarnation_id(&graph_dir, document_id).unwrap_err();

        assert!(error.contains("document directory disappeared"));
        assert!(!document_path.exists());
    }
}

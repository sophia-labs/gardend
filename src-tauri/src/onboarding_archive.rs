use crate::app_error::{AppError, AppResult};
use flate2::read::GzDecoder;
use serde::Deserialize;
use std::io::{Cursor, Read};
use tar::Archive;

pub(crate) const ARCHIVE_NAME: &str = "onboarding-v2.tar.gz";
pub(crate) const EXPECTED_FORMAT: &str = "mnemosyne-graph-export";

const ARCHIVE_BYTES: &[u8] = include_bytes!("../resources/onboarding/onboarding-v2.tar.gz");

#[derive(Debug, Deserialize)]
pub(crate) struct OnboardingManifest {
    pub version: u32,
    pub format: String,
    #[serde(default)]
    pub source_graph_title: Option<String>,
    #[serde(default)]
    pub source_graph_description: Option<String>,
}

pub(crate) struct OnboardingArchive {
    pub manifest: OnboardingManifest,
    pub workspace_bytes: Vec<u8>,
    pub documents: Vec<(String, Vec<u8>)>,
}

pub(crate) fn extract() -> AppResult<OnboardingArchive> {
    let cursor = Cursor::new(ARCHIVE_BYTES);
    let decoder = GzDecoder::new(cursor);
    let mut archive = Archive::new(decoder);

    let mut manifest: Option<OnboardingManifest> = None;
    let mut workspace_bytes: Option<Vec<u8>> = None;
    let mut documents: Vec<(String, Vec<u8>)> = Vec::new();

    let entries = archive
        .entries()
        .map_err(|error| AppError::storage(format!("read onboarding tar entries: {error}")))?;
    for entry_result in entries {
        let mut entry = entry_result
            .map_err(|error| AppError::storage(format!("read onboarding tar entry: {error}")))?;
        let path = entry
            .path()
            .map_err(|error| AppError::storage(format!("read tar entry path: {error}")))?
            .into_owned();
        let path_str = path.to_string_lossy().to_string();
        let normalized = path_str.trim_start_matches("./");

        let mut buf = Vec::new();
        entry.read_to_end(&mut buf).map_err(|error| {
            AppError::storage(format!("read tar entry bytes ({normalized}): {error}"))
        })?;

        if normalized == "manifest.json" {
            let parsed: OnboardingManifest = serde_json::from_slice(&buf).map_err(|error| {
                AppError::serialization(format!("parse onboarding manifest.json: {error}"))
            })?;
            manifest = Some(parsed);
        } else if normalized == "crdt/workspace.yjs" {
            workspace_bytes = Some(buf);
        } else if let Some(rest) = normalized.strip_prefix("crdt/documents/") {
            if let Some(doc_id) = rest.strip_suffix(".yjs") {
                if !doc_id.is_empty() {
                    documents.push((doc_id.to_string(), buf));
                }
            }
        }
        // Other archive members (rdf/graph.nq, artifacts/*) are intentionally
        // ignored in v1. The frontend warm-pass regenerates projection RDF
        // from the loaded Y.Docs in Garden's local URI scheme.
    }

    let manifest = manifest
        .ok_or_else(|| AppError::serialization("onboarding archive missing manifest.json"))?;
    let workspace_bytes = workspace_bytes
        .ok_or_else(|| AppError::storage("onboarding archive missing crdt/workspace.yjs"))?;

    if manifest.format != EXPECTED_FORMAT {
        return Err(AppError::validation(format!(
            "unexpected onboarding archive format: {} (expected {EXPECTED_FORMAT})",
            manifest.format,
        )));
    }

    documents.sort_by(|left, right| left.0.cmp(&right.0));

    Ok(OnboardingArchive {
        manifest,
        workspace_bytes,
        documents,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bundled_archive_extracts_with_documents_and_workspace() {
        let archive = extract().expect("extract bundled onboarding archive");
        assert_eq!(archive.manifest.format, EXPECTED_FORMAT);
        assert!(!archive.workspace_bytes.is_empty(), "workspace.yjs empty");
        assert!(
            !archive.documents.is_empty(),
            "no per-document Y.Doc bytes extracted"
        );
        assert!(
            archive
                .documents
                .iter()
                .any(|(doc_id, _)| doc_id == "garden-101"),
            "garden-101 not present in archive"
        );
    }
}

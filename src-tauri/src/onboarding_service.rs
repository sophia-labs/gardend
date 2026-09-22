use crate::{
    app_error::{AppError, AppResult},
    clock::timestamp,
    graph_service::{create_graph_service, CreateGraphInput},
    onboarding_archive::{self, ARCHIVE_NAME},
    paths::{
        document_dir, document_ydoc_dir, document_ydoc_state_path, ensure_document_dirs,
        graphs_dir, workspace_ydoc_state_path,
    },
    rdf::document_subject,
    runtime_config::{DOCUMENT_SCHEMA_VERSION, LOCAL_GRAPH_ORIGIN, LOCAL_PROVIDER_ID},
    storage::{create_dir_all, display_path, read_json, write_bytes, write_json},
};
use serde::{Deserialize, Serialize};
use serde_json::json;
use tauri::AppHandle;

const DEFAULT_ONBOARDING_GRAPH_ID: &str = "onboarding";
const SEEDED_MARKER_FILE: &str = "seeded.json";

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct SeedOnboardingInput {
    #[serde(default, alias = "graph_id")]
    pub graph_id: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct SeedReport {
    pub graph_id: String,
    pub document_ids: Vec<String>,
    pub archive_name: String,
    pub archive_version: u32,
    pub already_seeded: bool,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SeededMarker {
    archive_name: String,
    archive_version: u32,
    seeded_at: String,
    document_ids: Vec<String>,
}

#[cfg_attr(feature = "desktop", tauri::command)]
pub(crate) fn seed_onboarding_graph(
    app: AppHandle,
    input: SeedOnboardingInput,
) -> Result<SeedReport, String> {
    seed_onboarding_graph_service(&app, input).map_err(AppError::message)
}

pub(crate) fn seed_onboarding_graph_service(
    app: &AppHandle,
    input: SeedOnboardingInput,
) -> AppResult<SeedReport> {
    let graph_id = input
        .graph_id
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or(DEFAULT_ONBOARDING_GRAPH_ID)
        .to_string();

    let graph_dir = graphs_dir(app).map_err(AppError::storage)?.join(&graph_id);
    let seeded_marker_path = graph_dir.join(SEEDED_MARKER_FILE);

    // Idempotency guard #1: seeded.json present → already done, return cached report.
    if seeded_marker_path.is_file() {
        let marker: SeededMarker = read_json(&seeded_marker_path)?;
        return Ok(SeedReport {
            graph_id,
            document_ids: marker.document_ids,
            archive_name: marker.archive_name,
            archive_version: marker.archive_version,
            already_seeded: true,
        });
    }

    // Idempotency guard #2: graph.json present but no marker → another tool created
    // a graph at this id. Don't clobber it.
    if graph_dir.join("graph.json").is_file() {
        return Err(AppError::conflict(format!(
            "graph already exists at id={graph_id} without an onboarding marker; \
             refusing to overwrite"
        )));
    }

    let archive = onboarding_archive::extract()?;

    let title = archive
        .manifest
        .source_graph_title
        .clone()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| "Garden Onboarding".to_string());
    let description = archive
        .manifest
        .source_graph_description
        .clone()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty());

    create_graph_service(
        app,
        CreateGraphInput {
            title,
            graph_id: Some(graph_id.clone()),
            description,
            operation_id: None,
        },
    )?;

    // Write workspace Y.Doc bytes raw. The frontend warm-pass opens this and
    // triggers `save_workspace`, which projects to workspace.json + RDF in
    // Garden's local URI scheme via the existing materializers.
    let workspace_path = workspace_ydoc_state_path(&graph_dir);
    if let Some(parent) = workspace_path.parent() {
        create_dir_all(parent)?;
    }
    write_bytes(&workspace_path, &archive.workspace_bytes)?;

    let now = timestamp();
    let mut document_ids: Vec<String> = Vec::with_capacity(archive.documents.len());
    for (doc_id, bytes) in &archive.documents {
        let ydoc_dir = document_ydoc_dir(&graph_dir, doc_id);
        create_dir_all(&ydoc_dir)?;
        let state_path = document_ydoc_state_path(&graph_dir, doc_id);
        write_bytes(&state_path, bytes)?;

        // Write a minimal document.json manifest so `read_document` sees the doc
        // (without this, the frontend's `getOrAttachDocumentChannel` catches the
        // missing manifest and calls `create_document` — which writes an empty
        // Y.Doc to update-v1.bin and clobbers our seeded content). We don't go
        // through `write_document_record` here because it calls
        // `write_document_state_files`, which would itself rewrite update-v1.bin
        // from the manifest's empty `ydoc_update_base64` field.
        //
        // The bin file is the source of truth for content — `read_document_record`
        // reads it and overrides the manifest's empty `ydoc_update_base64` on read.
        // Sidecar projections (tiptap.xml, blocks.json, etc.) will be regenerated
        // by the existing save pipeline the first time the user edits.
        ensure_document_dirs(&graph_dir, doc_id).map_err(AppError::storage)?;
        let manifest_path = document_dir(&graph_dir, doc_id)
            .map_err(AppError::storage)?
            .join("document.json");
        let manifest = json!({
            "documentId": doc_id,
            "graphId": graph_id,
            "title": doc_id,
            "revision": 0,
            "body": "",
            "origin": LOCAL_GRAPH_ORIGIN,
            "providerId": LOCAL_PROVIDER_ID,
            "localPath": display_path(&document_dir(&graph_dir, doc_id).map_err(AppError::storage)?),
            "rdfSubject": document_subject(doc_id),
            "createdAt": now,
            "updatedAt": now,
            "capabilities": [
                "document.local.read",
                "document.local.write",
                "document.local.materialize.rdf",
                "document.local.ydoc",
                "document.local.tiptap-tree",
            ],
            "schemaVersion": DOCUMENT_SCHEMA_VERSION,
            "tiptapXml": "",
            "tiptapJson": null,
            "ydocUpdateBase64": "",
            "ydocStatePath": display_path(&state_path),
            "tree": null,
            "blocks": [],
            "rdfTripleCount": 0,
        });
        write_json(&manifest_path, &manifest)?;

        document_ids.push(doc_id.clone());
    }

    let marker = SeededMarker {
        archive_name: ARCHIVE_NAME.to_string(),
        archive_version: archive.manifest.version,
        seeded_at: timestamp(),
        document_ids: document_ids.clone(),
    };
    write_json(&seeded_marker_path, &marker)?;

    log::info!(
        "onboarding graph seeded: graph_id={} archive={} documents={} version={}",
        graph_id,
        ARCHIVE_NAME,
        document_ids.len(),
        archive.manifest.version,
    );

    Ok(SeedReport {
        graph_id,
        document_ids,
        archive_name: ARCHIVE_NAME.to_string(),
        archive_version: archive.manifest.version,
        already_seeded: false,
    })
}

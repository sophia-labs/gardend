pub(crate) use crate::ingestion_approach_types::IngestionApproachDescriptor;
use crate::{
    ingestion_approach_static::{
        plain_text_ingestion_approach, static_ingestion_approaches_before_pdf_runtime,
    },
    pdf_pipeline::{pdf_pipeline_engine_catalog, PDF_PYMUPDF_ENGINE_ID},
    pdf_runtimes::DoclingRuntimeStatus,
};

pub(crate) fn ingestion_approach_catalog(
    docling_status: Option<&DoclingRuntimeStatus>,
) -> Vec<IngestionApproachDescriptor> {
    let docling_catalog_status = docling_status
        .map(|status| status.status.as_str())
        .unwrap_or("setup-required");
    let docling_available = docling_status
        .map(|status| status.available)
        .unwrap_or(false);
    let docling_setup_required = docling_status
        .map(|status| status.setup_required)
        .unwrap_or(true);
    let docling_runtime = if docling_available {
        "local-python-docling"
    } else if docling_catalog_status == "unsupported" {
        "local-python-docling-unsupported"
    } else {
        "local-python-docling-setup-required"
    };
    let docling_dependency_summary = docling_status
        .and_then(|status| status.docling_version.as_ref())
        .map(|version| format!("Docling {version} via Python runtime"))
        .unwrap_or_else(|| "Docling Python runtime".to_string());
    let mut docling_limitations = docling_status
        .map(|status| status.limitations.clone())
        .unwrap_or_else(|| vec!["Docling runtime status has not been loaded".to_string()]);
    if docling_available {
        docling_limitations.push(
            "accurate PDF parsing runs as a local Python/Docling runtime and can take minutes on CPU"
                .to_string(),
        );
    }
    let pymupdf_engine = docling_status.and_then(|status| {
        pdf_pipeline_engine_catalog(status)
            .into_iter()
            .find(|engine| engine.engine_id == PDF_PYMUPDF_ENGINE_ID)
    });
    let pymupdf_status = pymupdf_engine
        .as_ref()
        .map(|engine| engine.status.clone())
        .unwrap_or_else(|| "setup-required".to_string());
    let pymupdf_setup_required = pymupdf_engine
        .as_ref()
        .map(|engine| engine.setup_required)
        .unwrap_or(true);
    let pymupdf_selectable = pymupdf_engine
        .as_ref()
        .map(|engine| engine.available)
        .unwrap_or(false);
    let pymupdf_limitations = pymupdf_engine
        .as_ref()
        .map(|engine| engine.limitations.clone())
        .unwrap_or_else(|| vec!["PyMuPDF4LLM runtime status has not been loaded".to_string()]);

    let mut catalog = static_ingestion_approaches_before_pdf_runtime();
    catalog.extend([
        IngestionApproachDescriptor {
            approach_id: "pdf.docling-accurate".to_string(),
            label: "PDF Accurate Docling".to_string(),
            family: "pdf".to_string(),
            status: docling_catalog_status.to_string(),
            runtime: docling_runtime.to_string(),
            file_types: vec!["pdf".to_string()],
            mime_types: vec!["application/pdf".to_string()],
            speed: "slow".to_string(),
            fidelity: "high".to_string(),
            output: "markdown-bridged-tiptap-json".to_string(),
            setup_required: docling_setup_required,
            selectable: docling_available,
            default_for: vec![],
            supports_ocr: true,
            supports_tables: true,
            supports_page_anchors: true,
            supports_original_view: true,
            supports_source_annotations: false,
            dependency_summary: docling_dependency_summary,
            best_for: vec![
                "structured reports".to_string(),
                "scanned or layout-heavy PDFs".to_string(),
            ],
            limitations: docling_limitations,
        },
        IngestionApproachDescriptor {
            approach_id: PDF_PYMUPDF_ENGINE_ID.to_string(),
            label: "PDF Structured PyMuPDF4LLM".to_string(),
            family: "pdf".to_string(),
            status: pymupdf_status,
            runtime: "local-python-pymupdf4llm".to_string(),
            file_types: vec!["pdf".to_string()],
            mime_types: vec!["application/pdf".to_string()],
            speed: "slow".to_string(),
            fidelity: "medium-high".to_string(),
            output: "markdown-bridged-tiptap-json".to_string(),
            setup_required: pymupdf_setup_required,
            selectable: pymupdf_selectable,
            default_for: vec![],
            supports_ocr: false,
            supports_tables: true,
            supports_page_anchors: true,
            supports_original_view: true,
            supports_source_annotations: false,
            dependency_summary: "PyMuPDF4LLM Python runtime or uv on-demand environment"
                .to_string(),
            best_for: vec![
                "searchable academic PDFs".to_string(),
                "column-heavy technical papers".to_string(),
                "structured Markdown imports".to_string(),
            ],
            limitations: pymupdf_limitations,
        },
    ]);
    catalog.push(plain_text_ingestion_approach());
    catalog
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn catalog_includes_local_pdf_options_without_runtime_status() {
        let catalog = ingestion_approach_catalog(None);
        let ids = catalog
            .iter()
            .map(|approach| approach.approach_id.as_str())
            .collect::<Vec<_>>();

        assert!(ids.contains(&"pdf.fast-text"));
        assert!(ids.contains(&"pdf.docling-accurate"));
        assert!(ids.contains(&PDF_PYMUPDF_ENGINE_ID));
    }
}

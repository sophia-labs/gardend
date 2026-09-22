pub(crate) use crate::pdf_pipeline_engine_specs::pdf_pipeline_engine_specs;

pub(crate) const PDF_PIPELINE_AUTO_ENGINE_ID: &str = "auto";
pub(crate) const PDF_FAST_TEXT_ENGINE_ID: &str = "pdf.fast-text";
pub(crate) const PDF_DOCLING_ENGINE_ID: &str = "pdf.docling-accurate";
pub(crate) const PDF_PYMUPDF_ENGINE_ID: &str = "pdf.pymupdf4llm";
pub(crate) const PDF_OCRMYPDF_ENGINE_ID: &str = "pdf.ocrmypdf-tesseract";
pub(crate) const PDF_GROBID_ENGINE_ID: &str = "pdf.grobid-paper";

#[derive(Debug, Clone, Copy)]
pub(crate) enum PdfEngineRuntimeKind {
    BuiltIn,
    Docling,
    PythonModuleOrUv {
        env_var: &'static str,
        module_name: &'static str,
        command_candidates: &'static [&'static str],
        uv_packages: &'static [&'static str],
    },
    CliCommands {
        commands: &'static [&'static str],
    },
    ServiceUrl {
        env_vars: &'static [&'static str],
    },
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct PdfEngineSpec {
    pub(crate) engine_id: &'static str,
    pub(crate) label: &'static str,
    pub(crate) preference_label: &'static str,
    pub(crate) preference_description: &'static str,
    pub(crate) runtime: &'static str,
    pub(crate) role: &'static str,
    pub(crate) speed: &'static str,
    pub(crate) fidelity: &'static str,
    pub(crate) output: &'static str,
    pub(crate) implemented: bool,
    pub(crate) selectable: bool,
    pub(crate) auto_select: bool,
    pub(crate) auto_priority: u8,
    pub(crate) supports_ocr: bool,
    pub(crate) supports_tables: bool,
    pub(crate) supports_page_anchors: bool,
    pub(crate) supports_original_view: bool,
    pub(crate) supports_source_annotations: bool,
    pub(crate) best_for: &'static [&'static str],
    pub(crate) capabilities: &'static [&'static str],
    pub(crate) limitations: &'static [&'static str],
    pub(crate) setup_hint: Option<&'static str>,
    pub(crate) adapter_pending_reason: Option<&'static str>,
    pub(crate) runtime_kind: PdfEngineRuntimeKind,
}

pub(crate) fn known_pdf_pipeline_engine_ids() -> Vec<&'static str> {
    let mut ids = vec![PDF_PIPELINE_AUTO_ENGINE_ID];
    ids.extend(
        pdf_pipeline_engine_specs()
            .into_iter()
            .map(|spec| spec.engine_id),
    );
    ids
}

pub(crate) fn normalize_pdf_pipeline_engine_id(value: &str) -> String {
    let normalized = value.trim().to_ascii_lowercase();
    if normalized.is_empty() {
        PDF_PIPELINE_AUTO_ENGINE_ID.to_string()
    } else {
        normalized
    }
}

pub(crate) fn validate_pdf_pipeline_engine_id(engine_id: &str) -> Result<(), String> {
    let known_engine_ids = known_pdf_pipeline_engine_ids();
    if known_engine_ids.iter().any(|known| *known == engine_id) {
        Ok(())
    } else {
        Err(format!(
            "unknown PDF ingestion engine '{engine_id}'. Expected one of: {}",
            known_engine_ids.join(", ")
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pdf_pipeline_engine_ids_are_known() {
        let ids = known_pdf_pipeline_engine_ids();

        assert!(ids.contains(&PDF_FAST_TEXT_ENGINE_ID));
        assert!(ids.contains(&PDF_DOCLING_ENGINE_ID));
        assert!(ids.contains(&PDF_PYMUPDF_ENGINE_ID));
    }

    #[test]
    fn pdf_pipeline_engine_normalization_handles_empty_and_case() {
        assert_eq!(
            normalize_pdf_pipeline_engine_id(""),
            PDF_PIPELINE_AUTO_ENGINE_ID
        );
        assert_eq!(
            normalize_pdf_pipeline_engine_id(" PDF.PYMUPDF4LLM "),
            PDF_PYMUPDF_ENGINE_ID
        );
    }

    #[test]
    fn pdf_pipeline_engine_validation_rejects_unknown_values() {
        let error = validate_pdf_pipeline_engine_id("pdf.unknown")
            .expect_err("unknown engines should be rejected");

        assert!(error.contains("unknown PDF ingestion engine"));
    }
}

use crate::{
    pdf_pipeline_catalog::{
        pdf_pipeline_engine_specs, PDF_FAST_TEXT_ENGINE_ID, PDF_PIPELINE_AUTO_ENGINE_ID,
    },
    pdf_pipeline_descriptors::{
        pdf_engine_descriptor_by_id, pdf_engine_unavailable_summary, PdfIngestionEngineDescriptor,
    },
};

pub(crate) fn resolve_pdf_pipeline_effective_engine(
    preferred_engine_id: &str,
    engines: &[PdfIngestionEngineDescriptor],
) -> (String, String) {
    let fast_text = pdf_engine_descriptor_by_id(engines, PDF_FAST_TEXT_ENGINE_ID);
    if preferred_engine_id == PDF_PIPELINE_AUTO_ENGINE_ID {
        if let Some((_, engine)) = pdf_pipeline_engine_specs()
            .into_iter()
            .filter(|spec| spec.auto_select)
            .filter_map(|spec| {
                pdf_engine_descriptor_by_id(engines, spec.engine_id)
                    .filter(|engine| engine.available)
                    .map(|engine| (spec.auto_priority, engine))
            })
            .min_by_key(|(priority, _)| *priority)
        {
            let reason = if engine.engine_id == PDF_FAST_TEXT_ENGINE_ID {
                "Auto selected PDF Fast Text because it is the available local baseline."
                    .to_string()
            } else {
                format!(
                    "Auto selected {} because its local runtime and adapter are available.",
                    engine.label
                )
            };
            return (engine.engine_id.clone(), reason);
        }
        return (
            PDF_FAST_TEXT_ENGINE_ID.to_string(),
            "Auto could not find an available parser; PDF Fast Text remains the fallback."
                .to_string(),
        );
    }

    let Some(preferred_engine) = pdf_engine_descriptor_by_id(engines, preferred_engine_id) else {
        return (
            PDF_FAST_TEXT_ENGINE_ID.to_string(),
            "Unknown preference was ignored; PDF Fast Text will run.".to_string(),
        );
    };
    if preferred_engine.available {
        return (
            preferred_engine.engine_id.clone(),
            format!(
                "Preference pins {}; the local runtime and adapter are available.",
                preferred_engine.label
            ),
        );
    }

    let fallback_label = fast_text
        .map(|engine| engine.label.as_str())
        .unwrap_or("PDF Fast Text");
    (
        preferred_engine
            .fallback_engine_id
            .clone()
            .unwrap_or_else(|| PDF_FAST_TEXT_ENGINE_ID.to_string()),
        format!(
            "{} is preferred but {}, so {} will run.",
            preferred_engine.label,
            pdf_engine_unavailable_summary(preferred_engine),
            fallback_label
        ),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        pdf_pipeline_catalog::{PDF_DOCLING_ENGINE_ID, PDF_PYMUPDF_ENGINE_ID},
        pdf_pipeline_descriptors::pdf_pipeline_engine_catalog,
        pdf_runtimes::DoclingRuntimeStatus,
    };

    fn docling_status(available: bool) -> DoclingRuntimeStatus {
        DoclingRuntimeStatus {
            runtime_id: "docling-python".to_string(),
            approach_id: PDF_DOCLING_ENGINE_ID.to_string(),
            package_name: "docling".to_string(),
            package_requirement: "docling>=2.0.0".to_string(),
            prepared: available,
            available,
            supported: true,
            setup_required: !available,
            setup_path: "/tmp/docling-runtime.json".to_string(),
            cache_policy: "uv-extra-pdf-accurate".to_string(),
            cache_path: "/tmp/uv-cache".to_string(),
            helper_path: "/tmp/docling-helper.py".to_string(),
            python_executable: available.then(|| "/tmp/python".to_string()),
            docling_version: available.then(|| "2.0.0".to_string()),
            prepared_at: available.then(|| "1".to_string()),
            platform: "test".to_string(),
            status: if available {
                "available".to_string()
            } else {
                "setup-required".to_string()
            },
            reason: (!available).then(|| "Docling runtime has not been prepared".to_string()),
            limitations: Vec::new(),
        }
    }

    #[test]
    fn auto_resolution_prefers_available_accurate_engine() {
        let engines = pdf_pipeline_engine_catalog(&docling_status(true));

        let (engine_id, reason) =
            resolve_pdf_pipeline_effective_engine(PDF_PIPELINE_AUTO_ENGINE_ID, &engines);

        assert_eq!(engine_id, PDF_DOCLING_ENGINE_ID);
        assert!(reason.contains("Auto selected Docling Accurate"));
    }

    #[test]
    fn preferred_unavailable_engine_falls_back_to_fast_text() {
        let engines = pdf_pipeline_engine_catalog(&docling_status(false));

        let (engine_id, reason) =
            resolve_pdf_pipeline_effective_engine(PDF_DOCLING_ENGINE_ID, &engines);

        assert_eq!(engine_id, PDF_FAST_TEXT_ENGINE_ID);
        assert!(reason.contains("Docling Accurate is preferred but"));
        assert!(reason.contains("PDF Fast Text will run"));
    }

    #[test]
    fn unknown_preference_uses_fast_text() {
        let engines = pdf_pipeline_engine_catalog(&docling_status(false));

        let (engine_id, reason) = resolve_pdf_pipeline_effective_engine("pdf.unknown", &engines);

        assert_eq!(engine_id, PDF_FAST_TEXT_ENGINE_ID);
        assert_eq!(
            reason,
            "Unknown preference was ignored; PDF Fast Text will run."
        );
    }

    #[test]
    fn available_pinned_engine_stays_effective() {
        let mut engines = pdf_pipeline_engine_catalog(&docling_status(false));
        for engine in &mut engines {
            if engine.engine_id == PDF_PYMUPDF_ENGINE_ID {
                engine.available = true;
                engine.runtime_available = true;
                engine.runtime_status = "runtime-ready".to_string();
                engine.reason = None;
                engine.runtime_reason = None;
            }
        }

        let (engine_id, reason) =
            resolve_pdf_pipeline_effective_engine(PDF_PYMUPDF_ENGINE_ID, &engines);

        assert_eq!(engine_id, PDF_PYMUPDF_ENGINE_ID);
        assert!(reason.contains("Preference pins PyMuPDF4LLM Structured"));
    }
}

use crate::{
    pdf_pipeline_catalog::{
        pdf_pipeline_engine_specs, PdfEngineSpec, PDF_DOCLING_ENGINE_ID, PDF_FAST_TEXT_ENGINE_ID,
    },
    pdf_pipeline_runtime::pdf_engine_runtime_state,
    pdf_runtimes::DoclingRuntimeStatus,
};
use serde::Serialize;

#[derive(Debug, Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub(crate) struct PdfIngestionPreferenceOption {
    pub(crate) engine_id: String,
    pub(crate) label: String,
    pub(crate) description: String,
}

#[derive(Debug, Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub(crate) struct PdfIngestionEngineDescriptor {
    pub(crate) engine_id: String,
    pub(crate) label: String,
    pub(crate) status: String,
    pub(crate) available: bool,
    pub(crate) implemented: bool,
    pub(crate) selectable: bool,
    pub(crate) runtime_available: bool,
    pub(crate) runtime_status: String,
    pub(crate) runtime_reason: Option<String>,
    pub(crate) fallback_engine_id: Option<String>,
    pub(crate) runtime: String,
    pub(crate) role: String,
    pub(crate) speed: String,
    pub(crate) fidelity: String,
    pub(crate) output: String,
    pub(crate) setup_required: bool,
    pub(crate) supports_ocr: bool,
    pub(crate) supports_tables: bool,
    pub(crate) supports_page_anchors: bool,
    pub(crate) supports_original_view: bool,
    pub(crate) supports_source_annotations: bool,
    pub(crate) best_for: Vec<String>,
    pub(crate) capabilities: Vec<String>,
    pub(crate) limitations: Vec<String>,
    pub(crate) setup_hint: Option<String>,
    pub(crate) reason: Option<String>,
}

pub(crate) fn pdf_pipeline_preference_options() -> Vec<PdfIngestionPreferenceOption> {
    let mut options = vec![PdfIngestionPreferenceOption {
        engine_id: crate::pdf_pipeline_catalog::PDF_PIPELINE_AUTO_ENGINE_ID.to_string(),
        label: "Auto".to_string(),
        description: "Use the best available local engine for this profile.".to_string(),
    }];
    options.extend(pdf_pipeline_engine_specs().into_iter().map(|spec| {
        PdfIngestionPreferenceOption {
            engine_id: spec.engine_id.to_string(),
            label: spec.preference_label.to_string(),
            description: spec.preference_description.to_string(),
        }
    }));
    options
}

pub(crate) fn pdf_pipeline_engine_catalog(
    docling_status: &DoclingRuntimeStatus,
) -> Vec<PdfIngestionEngineDescriptor> {
    pdf_pipeline_engine_specs()
        .into_iter()
        .map(|spec| pdf_engine_descriptor_from_spec(&spec, docling_status))
        .collect()
}

pub(crate) fn pdf_engine_descriptor_by_id<'a>(
    engines: &'a [PdfIngestionEngineDescriptor],
    engine_id: &str,
) -> Option<&'a PdfIngestionEngineDescriptor> {
    engines.iter().find(|engine| engine.engine_id == engine_id)
}

pub(crate) fn pdf_engine_unavailable_summary(engine: &PdfIngestionEngineDescriptor) -> String {
    if !engine.implemented && engine.runtime_available {
        return "its runtime is present but its execution adapter is not wired yet".to_string();
    }
    if !engine.implemented {
        return "its runtime is not ready and its execution adapter is not wired yet".to_string();
    }
    if !engine.runtime_available {
        return engine
            .runtime_reason
            .clone()
            .or_else(|| engine.reason.clone())
            .unwrap_or_else(|| "its local runtime is unavailable".to_string());
    }
    engine
        .reason
        .clone()
        .unwrap_or_else(|| "it is unavailable".to_string())
}

fn push_unique_string(values: &mut Vec<String>, value: String) {
    if !values.iter().any(|existing| existing == &value) {
        values.push(value);
    }
}

fn pdf_engine_descriptor_from_spec(
    spec: &PdfEngineSpec,
    docling_status: &DoclingRuntimeStatus,
) -> PdfIngestionEngineDescriptor {
    let runtime_state = pdf_engine_runtime_state(spec, docling_status);
    let available = spec.implemented && runtime_state.runtime_available;
    let status = if available {
        "available".to_string()
    } else if !spec.implemented && runtime_state.runtime_available {
        "candidate-runtime-ready".to_string()
    } else if !spec.implemented {
        "candidate-runtime-missing".to_string()
    } else {
        runtime_state.status.clone()
    };
    let mut limitations = spec
        .limitations
        .iter()
        .map(|value| value.to_string())
        .collect::<Vec<_>>();
    if spec.engine_id == PDF_DOCLING_ENGINE_ID {
        for limitation in &docling_status.limitations {
            push_unique_string(&mut limitations, limitation.clone());
        }
    }
    if !spec.implemented {
        if let Some(reason) = spec.adapter_pending_reason {
            push_unique_string(&mut limitations, reason.to_string());
        }
    }
    if !runtime_state.runtime_available {
        if let Some(reason) = runtime_state.reason.as_ref() {
            push_unique_string(&mut limitations, reason.clone());
        }
    }

    let mut reason_parts = Vec::new();
    if !available {
        if let Some(reason) = spec.adapter_pending_reason {
            reason_parts.push(reason.to_string());
        }
        if let Some(reason) = runtime_state.reason.as_ref() {
            reason_parts.push(reason.clone());
        }
    }
    let reason = if reason_parts.is_empty() {
        runtime_state.reason.clone().filter(|_| !available)
    } else {
        Some(reason_parts.join("; "))
    };

    PdfIngestionEngineDescriptor {
        engine_id: spec.engine_id.to_string(),
        label: spec.label.to_string(),
        status,
        available,
        implemented: spec.implemented,
        selectable: spec.selectable,
        runtime_available: runtime_state.runtime_available,
        runtime_status: runtime_state.status,
        runtime_reason: runtime_state.reason,
        fallback_engine_id: if available || spec.engine_id == PDF_FAST_TEXT_ENGINE_ID {
            None
        } else {
            Some(PDF_FAST_TEXT_ENGINE_ID.to_string())
        },
        runtime: spec.runtime.to_string(),
        role: spec.role.to_string(),
        speed: spec.speed.to_string(),
        fidelity: spec.fidelity.to_string(),
        output: spec.output.to_string(),
        setup_required: runtime_state.setup_required,
        supports_ocr: spec.supports_ocr,
        supports_tables: spec.supports_tables,
        supports_page_anchors: spec.supports_page_anchors,
        supports_original_view: spec.supports_original_view,
        supports_source_annotations: spec.supports_source_annotations,
        best_for: spec
            .best_for
            .iter()
            .map(|value| value.to_string())
            .collect(),
        capabilities: spec
            .capabilities
            .iter()
            .map(|value| value.to_string())
            .collect(),
        limitations,
        setup_hint: runtime_state
            .setup_hint
            .or_else(|| spec.setup_hint.map(|value| value.to_string())),
        reason,
    }
}

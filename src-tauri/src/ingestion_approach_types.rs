use serde::Serialize;

#[derive(Debug, Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub(crate) struct IngestionApproachDescriptor {
    pub(crate) approach_id: String,
    pub(crate) label: String,
    pub(crate) family: String,
    pub(crate) status: String,
    pub(crate) runtime: String,
    pub(crate) file_types: Vec<String>,
    pub(crate) mime_types: Vec<String>,
    pub(crate) speed: String,
    pub(crate) fidelity: String,
    pub(crate) output: String,
    pub(crate) setup_required: bool,
    pub(crate) selectable: bool,
    pub(crate) default_for: Vec<String>,
    pub(crate) supports_ocr: bool,
    pub(crate) supports_tables: bool,
    pub(crate) supports_page_anchors: bool,
    pub(crate) supports_original_view: bool,
    pub(crate) supports_source_annotations: bool,
    pub(crate) dependency_summary: String,
    pub(crate) best_for: Vec<String>,
    pub(crate) limitations: Vec<String>,
}

use serde::{Deserialize, Serialize};

pub(crate) use crate::semantic_model_catalog::semantic_model_spec_by_id;
pub(crate) use crate::semantic_model_state::{
    read_semantic_model_config, semantic_model_effective_batch_size, write_semantic_model_config,
    write_semantic_model_setup_manifest,
};
pub(crate) use crate::semantic_model_status::{
    semantic_model_catalog, semantic_model_status, SemanticModelDescriptor, SemanticModelStatus,
};

pub(crate) const SEMANTIC_INDEX_SCHEMA_VERSION: u32 = 1;
pub(crate) const SEMANTIC_MODEL_CACHE_POLICY: &str = "hf-hub-user-cache";

#[derive(Debug, Deserialize, Clone)]
#[serde(rename_all = "camelCase")]
pub(crate) struct SemanticModelConfigInput {
    pub(crate) model_id: String,
    #[serde(default)]
    pub(crate) batch_size: Option<usize>,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
#[serde(rename_all = "camelCase")]
pub(crate) struct SemanticModelSelectionConfig {
    pub(crate) schema_version: u32,
    pub(crate) selected_model_id: String,
    pub(crate) batch_size: Option<usize>,
    pub(crate) updated_at: String,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
#[serde(rename_all = "camelCase")]
pub(crate) struct SemanticModelSetupManifest {
    pub(crate) schema_version: u32,
    pub(crate) provider_id: String,
    pub(crate) model_id: String,
    pub(crate) hf_repo: String,
    pub(crate) dimensions: usize,
    pub(crate) max_tokens: usize,
    pub(crate) cache_policy: String,
    pub(crate) cache_path: String,
    pub(crate) prepared_at: String,
}

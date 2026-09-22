use serde::{Deserialize, Serialize};

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct CreateLoopbackClientTokenInput {
    pub(crate) label: Option<String>,
    #[serde(alias = "grant_profile_id")]
    pub(crate) grant_profile_id: Option<String>,
    pub(crate) scopes: Option<Vec<String>>,
    #[serde(alias = "expires_in_days")]
    pub(crate) expires_in_days: Option<u32>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct RevokeLoopbackClientTokenInput {
    #[serde(alias = "token_id")]
    pub(crate) token_id: String,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct CreateLoopbackClientTokenResponse {
    pub(crate) token: String,
    pub(crate) record: LoopbackClientTokenSummary,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct LoopbackClientTokenSummary {
    pub(crate) token_id: String,
    pub(crate) label: String,
    pub(crate) grant_profile_id: Option<String>,
    pub(crate) scopes: Vec<String>,
    pub(crate) created_at: String,
    pub(crate) expires_at: String,
    pub(crate) revoked_at: Option<String>,
    pub(crate) active: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct LoopbackClientTokenRecord {
    pub(crate) token_id: String,
    pub(crate) label: String,
    #[serde(default)]
    pub(crate) token_hash: String,
    pub(crate) grant_profile_id: Option<String>,
    pub(crate) scopes: Vec<String>,
    pub(crate) created_at: String,
    #[serde(default)]
    pub(crate) expires_at: String,
    #[serde(default)]
    pub(crate) revoked_at: Option<String>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct LoopbackClientTokenStore {
    #[serde(default)]
    pub(crate) tokens: Vec<LoopbackClientTokenRecord>,
}

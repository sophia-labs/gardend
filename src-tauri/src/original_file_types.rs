use crate::clock::parse_timestamp;
use serde::{Deserialize, Serialize};

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct SaveOriginalFileInput {
    pub(crate) graph_id: String,
    pub(crate) document_id: String,
    pub(crate) filename: String,
    pub(crate) mime_type: String,
    pub(crate) data_base64: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct AdoptPendingOriginalFileInput {
    pub(crate) graph_id: String,
    pub(crate) document_id: String,
    pub(crate) pending_path: String,
    pub(crate) filename: String,
    pub(crate) mime_type: String,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
#[serde(rename_all = "camelCase")]
pub(crate) struct OriginalFileManifest {
    pub(crate) filename: String,
    /// Exact imported name as data only; `filename` remains the safe storage key.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) source_filename: Option<String>,
    pub(crate) mime_type: String,
    pub(crate) size_bytes: usize,
    pub(crate) local_path: String,
    pub(crate) created_at: String,
    pub(crate) updated_at: String,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct OriginalFileRecord {
    pub(crate) graph_id: String,
    pub(crate) document_id: String,
    pub(crate) filename: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) source_filename: Option<String>,
    pub(crate) mime_type: String,
    pub(crate) size_bytes: usize,
    pub(crate) local_path: String,
    pub(crate) updated_at: String,
    pub(crate) data_base64: String,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct OriginalFileManifestRecord {
    pub(crate) graph_id: String,
    pub(crate) document_id: String,
    pub(crate) filename: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) source_filename: Option<String>,
    pub(crate) mime_type: String,
    pub(crate) size_bytes: usize,
    pub(crate) local_path: String,
    pub(crate) updated_at: String,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ImageAccessTokenManifest {
    pub(crate) token: String,
    pub(crate) created_at: String,
    #[serde(default)]
    pub(crate) expires_at: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ImageAccessToken {
    pub(crate) token: String,
    pub(crate) expires_at: String,
}

pub(crate) const IMAGE_ACCESS_TOKEN_TTL_MS: u128 = 24 * 60 * 60 * 1000;

impl ImageAccessTokenManifest {
    pub(crate) fn expires_at_ms(&self) -> Option<u128> {
        parse_timestamp(self.expires_at.trim()).or_else(|| {
            parse_timestamp(self.created_at.trim())
                .map(|created_at| created_at.saturating_add(IMAGE_ACCESS_TOKEN_TTL_MS))
        })
    }

    pub(crate) fn matches(
        &self,
        candidate: Option<&String>,
        candidate_expires_at: Option<&String>,
        now_ms: u128,
    ) -> bool {
        let Some(candidate) = candidate
            .map(|value| value.trim())
            .filter(|value| !value.is_empty())
        else {
            return false;
        };
        if self.token != candidate {
            return false;
        }

        let Some(expires_at_ms) = self.expires_at_ms() else {
            return false;
        };
        if now_ms > expires_at_ms {
            return false;
        }

        if let Some(candidate_expires_at) = candidate_expires_at
            .map(|value| value.trim())
            .filter(|value| !value.is_empty())
        {
            return parse_timestamp(candidate_expires_at) == Some(expires_at_ms);
        }

        true
    }
}

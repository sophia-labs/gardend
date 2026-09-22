use crate::app_runtime::AppHandle;
use crate::{
    cell_graph_boundary::CellGraphBoundary, cell_lifecycle::CellLifecycle,
    local_jobs::LocalJobRegistry, local_service_host::LocalServiceHost,
    loopback_scopes::LoopbackScopeDetail, loopback_token_grants::LoopbackGrantProfile,
};
use serde::Serialize;
use std::sync::Arc;

#[derive(Debug, Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub(crate) struct LoopbackManifest {
    pub(crate) runtime_profile: &'static str,
    pub(crate) bind_host: &'static str,
    pub(crate) port: u16,
    pub(crate) api_url: String,
    pub(crate) mcp_url: String,
    pub(crate) openapi_url: String,
    pub(crate) token: String,
    pub(crate) pid: u32,
    pub(crate) started_at: String,
    pub(crate) manifest_path: String,
    pub(crate) auth_header: &'static str,
    pub(crate) token_audience: &'static str,
    pub(crate) token_storage: &'static str,
    pub(crate) security_warning: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) cell_graph_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) cell_owner: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) cell_generation: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) cell_registry_revision: Option<u64>,
    pub(crate) capabilities: Vec<&'static str>,
    pub(crate) token_scope_mode: &'static str,
    pub(crate) token_scopes: Vec<&'static str>,
    pub(crate) scope_details: Vec<LoopbackScopeDetail>,
    pub(crate) grant_profiles: Vec<LoopbackGrantProfile>,
}

/// Secret-free projection of [`LoopbackManifest`] returned by `GET /manifest`
/// for EVERY principal, including the Owner/master-token caller — the
/// redaction is per-route, not per-principal (P0-1,
/// `reports/oss-publication-readiness-20260921.md`). Disk `loopback.json`
/// keeps serializing the full `LoopbackManifest` (the legitimate owner-only
/// bootstrap channel via `write_secret_json`); only the HTTP response uses
/// this type. Deliberately drops `token`, `pid`, and `manifest_path` — none
/// of which any HTTP caller needs (the process that needs them reads the
/// file directly) and all of which are either the master bearer itself or
/// local-machine detail that helps an attacker correlate/target the process.
/// Mirrors the filtering precedent of `loopback_openapi`
/// (`loopback_core_routes.rs`), which also never echoes raw secrets to HTTP
/// callers.
#[derive(Debug, Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub(crate) struct LoopbackManifestPublic {
    pub(crate) runtime_profile: &'static str,
    pub(crate) bind_host: &'static str,
    pub(crate) port: u16,
    pub(crate) api_url: String,
    pub(crate) mcp_url: String,
    pub(crate) openapi_url: String,
    pub(crate) started_at: String,
    pub(crate) auth_header: &'static str,
    pub(crate) token_audience: &'static str,
    pub(crate) token_storage: &'static str,
    pub(crate) security_warning: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) cell_graph_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) cell_owner: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) cell_generation: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) cell_registry_revision: Option<u64>,
    pub(crate) capabilities: Vec<&'static str>,
    pub(crate) token_scope_mode: &'static str,
    pub(crate) token_scopes: Vec<&'static str>,
    pub(crate) scope_details: Vec<LoopbackScopeDetail>,
    pub(crate) grant_profiles: Vec<LoopbackGrantProfile>,
}

impl From<&LoopbackManifest> for LoopbackManifestPublic {
    fn from(manifest: &LoopbackManifest) -> Self {
        // Field-by-field (no `..manifest` shorthand possible across types):
        // any field added to `LoopbackManifest` in the future must be
        // deliberately triaged here rather than silently inherited.
        Self {
            runtime_profile: manifest.runtime_profile,
            bind_host: manifest.bind_host,
            port: manifest.port,
            api_url: manifest.api_url.clone(),
            mcp_url: manifest.mcp_url.clone(),
            openapi_url: manifest.openapi_url.clone(),
            started_at: manifest.started_at.clone(),
            auth_header: manifest.auth_header,
            token_audience: manifest.token_audience,
            token_storage: manifest.token_storage,
            security_warning: manifest.security_warning,
            cell_graph_id: manifest.cell_graph_id.clone(),
            cell_owner: manifest.cell_owner.clone(),
            cell_generation: manifest.cell_generation,
            cell_registry_revision: manifest.cell_registry_revision,
            capabilities: manifest.capabilities.clone(),
            token_scope_mode: manifest.token_scope_mode,
            token_scopes: manifest.token_scopes.clone(),
            scope_details: manifest.scope_details.clone(),
            grant_profiles: manifest.grant_profiles.clone(),
        }
    }
}

#[derive(Clone)]
pub(crate) struct LoopbackState {
    pub(crate) app: AppHandle,
    pub(crate) token: String,
    pub(crate) manifest: LoopbackManifest,
    pub(crate) jobs: Arc<LocalJobRegistry>,
    pub(crate) services: Arc<LocalServiceHost>,
    pub(crate) lifecycle: Arc<CellLifecycle>,
    pub(crate) cell_graph: Arc<CellGraphBoundary>,
}

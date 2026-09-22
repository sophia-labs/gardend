//! Single-graph ownership boundary for hosted `gardend` cells.
//!
//! Desktop Garden and ordinary local/headless runtimes remain multi-graph.
//! A pure headless process becomes a single-graph cell only when
//! `GARDEN_CELL_ID` is set. The owner is parsed once at startup, carried
//! as managed process state, mirrored in [`LoopbackState`], and consumed by
//! every state-selection boundary:
//!
//! - route middleware checks path-selected graphs and graph-owned jobs;
//! - [`CellGraphJson`] checks/injects JSON body selectors without a second
//!   parse or a second body allocation;
//! - MCP dispatch checks/injects the same owner before registry dispatch.
//! - durable enqueue/recovery and background schedulers reuse the same owner.
//!
//! The embedded policy is deliberately closed. In cell mode an unclassified
//! non-graph route fails closed at runtime; the parity checker enumerates both
//! the REST/OpenAPI surface and all MCP tools so drift fails before release.

use crate::{
    local_job_types::local_job_graph_id,
    loopback_http::{
        bearer_token, loopback_error, loopback_master_token_presented, origin_ok,
        require_loopback_scopes,
    },
    loopback_scopes::crdt_operation_scopes,
    loopback_state::LoopbackState,
};
use axum::{
    extract::{FromRef, FromRequest, MatchedPath, Request, State},
    http::{Method, StatusCode},
    middleware::Next,
    response::{IntoResponse, Response},
    Json,
};
use serde::de::DeserializeOwned;
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
};

const POLICY_JSON: &str = include_str!("cell_graph_boundary_policy.json");

#[cfg(all(test, feature = "headless"))]
#[path = "owned_restore_tests.rs"]
mod owned_restore_tests;
#[cfg(all(test, feature = "headless"))]
#[path = "loopback_manifest_secret_redaction_cell_tests.rs"]
mod loopback_manifest_secret_redaction_cell_tests;
const MCP_CATALOG_JSON: &str = include_str!("mcp_tool_catalog.json");
const EFFECT_REGISTRY_JSON: &str = include_str!("../../parity/local-loopback-surface.json");

#[derive(Debug, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct CellGraphBoundaryPolicy {
    rest_surface_sha256: String,
    mcp_surface_sha256: String,
    effect_registry_sha256: String,
    rest_global: BTreeSet<String>,
    rest_path_graph: BTreeSet<String>,
    rest_body_graph: BTreeSet<String>,
    rest_operation_graph: BTreeSet<String>,
    rest_job_graph: BTreeSet<String>,
    rest_profile_denied: BTreeSet<String>,
    rest_profile_denied_any_method: BTreeSet<String>,
    rest_job_handlers: BTreeMap<String, String>,
    mcp_global: BTreeSet<String>,
    mcp_job_graph: BTreeSet<String>,
    mcp_job_carriers: BTreeMap<String, Vec<String>>,
    mcp_operation_guarded: BTreeSet<String>,
    mcp_profile_denied: BTreeSet<String>,
    mcp_graph_scoped: BTreeSet<String>,
}

#[derive(Debug, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct EffectRegistry {
    routes: Vec<EffectEntry>,
    mcp_tools: Vec<EffectEntry>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct EffectEntry {
    #[serde(default)]
    method: Option<String>,
    #[serde(default)]
    path: Option<String>,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    scopes: Vec<String>,
    #[serde(default)]
    scope_mode: Option<String>,
}

#[derive(Debug, Clone)]
pub(crate) struct CellGraphBoundary {
    owner_graph_id: Option<String>,
    owner_principal: Option<String>,
    graph_generation: Option<u64>,
    registry_revision: Option<u64>,
    policy: Arc<CellGraphBoundaryPolicy>,
    rest_effects: Arc<BTreeMap<String, EffectEntry>>,
    mcp_effects: Arc<BTreeMap<String, EffectEntry>>,
    mcp_graph_scoped: Arc<BTreeSet<String>>,
    surface_drift: Arc<Vec<String>>,
    lease_secret: Option<Arc<Vec<u8>>>,
}

#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, serde::Serialize, serde::Deserialize,
)]
#[serde(rename_all = "lowercase")]
pub(crate) enum CellRole {
    Viewer,
    Editor,
    Owner,
}

#[derive(Debug, Clone, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct CellLeaseClaims {
    iss: String,
    aud: String,
    sub: String,
    owner: String,
    graph_id: String,
    generation: u64,
    cell_id: String,
    role: CellRole,
    policy_revision: u64,
    registry_revision: u64,
    session_id: String,
    iat: u64,
    exp: u64,
}

#[derive(Debug, Clone)]
pub(crate) struct VerifiedCellLease {
    pub(crate) principal: String,
    pub(crate) role: CellRole,
    pub(crate) policy_revision: u64,
}

tokio::task_local! {
    static CURRENT_CELL_LEASE: VerifiedCellLease;
}

pub(crate) fn current_cell_lease() -> Option<VerifiedCellLease> {
    CURRENT_CELL_LEASE.try_with(Clone::clone).ok()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RestPolicy {
    Global,
    BodyGraph,
    OperationGraph,
    JobGraph,
    ProfileDenied,
    PathGraph,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum McpPolicy {
    Global,
    JobGraph,
    OperationGuarded,
    ProfileDenied,
    GraphScoped,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum CellGraphBoundaryError {
    GraphMismatch,
    JobNotOwned,
    InsufficientRole(String),
    ProfileWideOperation(String),
    UnclassifiedRestRoute(String),
    UnclassifiedMcpTool(String),
    InvalidRequest(String),
}

impl CellGraphBoundaryError {
    pub(crate) fn mcp_code(&self) -> i64 {
        match self {
            Self::InvalidRequest(_) => -32602,
            Self::UnclassifiedRestRoute(_) | Self::UnclassifiedMcpTool(_) => -32603,
            Self::InsufficientRole(_) => -32003,
            Self::GraphMismatch | Self::JobNotOwned | Self::ProfileWideOperation(_) => -32004,
        }
    }

    pub(crate) fn mcp_message(&self) -> String {
        match self {
            Self::GraphMismatch | Self::JobNotOwned => "graph not found in this cell".to_string(),
            Self::InsufficientRole(operation) => {
                format!("cell role does not permit {operation}")
            }
            Self::ProfileWideOperation(operation) => {
                format!("operation {operation} is disabled in a single-graph gardend cell")
            }
            Self::UnclassifiedRestRoute(route) => {
                format!("cell graph boundary has no REST policy for {route}")
            }
            Self::UnclassifiedMcpTool(tool) => {
                format!("cell graph boundary has no MCP policy for {tool}")
            }
            Self::InvalidRequest(message) => message.clone(),
        }
    }

    pub(crate) fn http_response(&self) -> Response {
        match self {
            // Do not reveal the configured owner to a caller that selected a
            // different graph (including anonymous signed-image requests).
            Self::GraphMismatch | Self::JobNotOwned => {
                loopback_error(StatusCode::NOT_FOUND, "graph not found in this cell")
            }
            Self::InsufficientRole(operation) => loopback_error(
                StatusCode::FORBIDDEN,
                &format!("cell role does not permit {operation}"),
            ),
            Self::ProfileWideOperation(operation) => loopback_error(
                StatusCode::FORBIDDEN,
                &format!("operation {operation} is disabled in a single-graph gardend cell"),
            ),
            Self::InvalidRequest(message) => loopback_error(StatusCode::BAD_REQUEST, message),
            Self::UnclassifiedRestRoute(route) => loopback_error(
                StatusCode::SERVICE_UNAVAILABLE,
                &format!("cell graph boundary has no REST policy for {route}"),
            ),
            Self::UnclassifiedMcpTool(tool) => loopback_error(
                StatusCode::SERVICE_UNAVAILABLE,
                &format!("cell graph boundary has no MCP policy for {tool}"),
            ),
        }
    }
}

impl CellGraphBoundary {
    pub(crate) fn from_process_env() -> Result<Self, String> {
        #[cfg(all(feature = "headless", not(feature = "desktop")))]
        let binding = crate::cell_registry_authority::verified_binding()?;
        #[cfg(all(feature = "headless", not(feature = "desktop")))]
        let (owner_graph_id, owner_principal, graph_generation, registry_revision) = match binding {
            Some(binding) => (
                Some(binding.graph_id),
                Some(binding.owner),
                Some(binding.generation),
                Some(binding.registry_revision),
            ),
            None => (None, None, None, None),
        };
        #[cfg(any(not(feature = "headless"), feature = "desktop"))]
        let (owner_graph_id, owner_principal, graph_generation, registry_revision) =
            (None, None, None, None);

        let lease_secret = match std::env::var("GARDEN_CELL_LEASE_SECRET") {
            Ok(value) if value.as_bytes().len() >= 32 => Some(value.into_bytes()),
            Ok(_) => return Err("GARDEN_CELL_LEASE_SECRET must contain at least 32 bytes".into()),
            Err(std::env::VarError::NotPresent) if owner_graph_id.is_none() => None,
            Err(std::env::VarError::NotPresent) => {
                return Err("GARDEN_CELL_LEASE_SECRET is required for a bound gardend cell".into())
            }
            Err(std::env::VarError::NotUnicode(_)) => {
                return Err("GARDEN_CELL_LEASE_SECRET is not valid Unicode".into())
            }
        };
        Self::new_with_binding(
            owner_graph_id,
            owner_principal,
            graph_generation,
            registry_revision,
            lease_secret,
        )
    }

    fn new(owner_graph_id: Option<String>) -> Result<Self, String> {
        let enabled = owner_graph_id.is_some();
        Self::new_with_binding(
            owner_graph_id,
            enabled.then(|| "user:test-owner".to_string()),
            enabled.then_some(1),
            enabled.then_some(1),
            None,
        )
    }

    fn new_with_binding(
        owner_graph_id: Option<String>,
        owner_principal: Option<String>,
        graph_generation: Option<u64>,
        registry_revision: Option<u64>,
        lease_secret: Option<Vec<u8>>,
    ) -> Result<Self, String> {
        if let Some(graph_id) = owner_graph_id.as_deref() {
            crate::ids::validate_local_id(graph_id, "GARDEN_CELL_GRAPH_ID")
                .map_err(|error| format!("invalid GARDEN_CELL_GRAPH_ID: {error}"))?;
        }
        let policy: CellGraphBoundaryPolicy = serde_json::from_str(POLICY_JSON)
            .map_err(|error| format!("parse embedded cell graph boundary policy: {error}"))?;
        validate_disjoint_policy_sets(&policy)?;
        let (rest_effects, mcp_effects, mcp_graph_scoped, surface_drift) = if owner_graph_id
            .is_some()
        {
            // Surface drift is a deployment fault, not a reason to crash the
            // cell. Preserve the last explicit allow-list, quarantine anything
            // unknown at discovery and dispatch, and make the drift observable.
            // The parity gate performs these same checks as a fatal CI error.
            let (rest_effects, mcp_effects) = parse_effect_registry()?;
            let mut drift = Vec::new();
            if let Err(error) = validate_effect_registry(&policy) {
                drift.push(error);
            }
            if let Err(error) = validate_rest_openapi_surface(&policy, &rest_effects) {
                drift.push(error);
            }
            let mcp_graph_scoped = match validate_and_classify_mcp_catalog(&policy, &mcp_effects) {
                Ok(classified) => classified,
                Err(error) => {
                    drift.push(error);
                    known_catalog_tools()?
                        .intersection(&policy.mcp_graph_scoped)
                        .cloned()
                        .collect()
                }
            };
            for finding in &drift {
                log::error!(target: "cell_surface_drift", "{finding}");
            }
            (rest_effects, mcp_effects, mcp_graph_scoped, drift)
        } else {
            (
                BTreeMap::new(),
                BTreeMap::new(),
                BTreeSet::new(),
                Vec::new(),
            )
        };
        Ok(Self {
            owner_graph_id,
            owner_principal,
            graph_generation,
            registry_revision,
            policy: Arc::new(policy),
            rest_effects: Arc::new(rest_effects),
            mcp_effects: Arc::new(mcp_effects),
            mcp_graph_scoped: Arc::new(mcp_graph_scoped),
            surface_drift: Arc::new(surface_drift),
            lease_secret: lease_secret.map(Arc::new),
        })
    }

    #[cfg(test)]
    pub(crate) fn for_test(owner_graph_id: Option<&str>) -> Self {
        Self::new(owner_graph_id.map(str::to_string)).expect("test cell graph boundary")
    }

    #[cfg(test)]
    pub(crate) fn for_test_binding(
        owner_graph_id: &str,
        owner_principal: &str,
        graph_generation: u64,
    ) -> Self {
        Self::new_with_binding(
            Some(owner_graph_id.to_string()),
            Some(owner_principal.to_string()),
            Some(graph_generation),
            Some(1),
            None,
        )
        .expect("bound test cell graph boundary")
    }

    pub(crate) fn owner_graph_id(&self) -> Option<&str> {
        self.owner_graph_id.as_deref()
    }

    pub(crate) fn owner_principal(&self) -> Option<&str> {
        self.owner_principal.as_deref()
    }

    pub(crate) fn graph_generation(&self) -> Option<u64> {
        self.graph_generation
    }

    pub(crate) fn registry_revision(&self) -> Option<u64> {
        self.registry_revision
    }

    pub(crate) fn is_enabled(&self) -> bool {
        self.owner_graph_id.is_some()
    }

    pub(crate) fn surface_drift(&self) -> &[String] {
        &self.surface_drift
    }

    pub(crate) fn authorize_graph_id(
        &self,
        requested_graph_id: &str,
    ) -> Result<(), CellGraphBoundaryError> {
        let Some(owner_graph_id) = self.owner_graph_id() else {
            return Ok(());
        };
        if requested_graph_id.trim() == owner_graph_id {
            Ok(())
        } else {
            Err(CellGraphBoundaryError::GraphMismatch)
        }
    }

    /// F4c may repair only the graph this process was bound to at startup.
    /// Keep the public error indistinguishable from an ordinary missing graph:
    /// the configured owner is authority, not response data.
    #[cfg(all(feature = "headless", not(feature = "desktop")))]
    pub(crate) fn authorize_graph_self_heal(&self, graph_id: &str) -> Result<(), String> {
        self.authorize_graph_id(graph_id)
            .map_err(|_| format!("graph not found: {graph_id}"))
    }

    fn rest_policy(
        &self,
        method: &Method,
        matched_path: &str,
    ) -> Result<RestPolicy, CellGraphBoundaryError> {
        let method = if method == Method::HEAD {
            Method::GET
        } else {
            method.clone()
        };
        let key = format!("{method} {matched_path}");
        if self.policy.rest_profile_denied.contains(&key)
            || self
                .policy
                .rest_profile_denied_any_method
                .contains(matched_path)
        {
            return Ok(RestPolicy::ProfileDenied);
        }
        if self.policy.rest_job_graph.contains(&key) {
            return Ok(RestPolicy::JobGraph);
        }
        if self.policy.rest_operation_graph.contains(&key) {
            return Ok(RestPolicy::OperationGraph);
        }
        if self.policy.rest_body_graph.contains(&key) {
            return Ok(RestPolicy::BodyGraph);
        }
        if self.policy.rest_global.contains(&key) {
            return Ok(RestPolicy::Global);
        }
        if self.policy.rest_path_graph.contains(&key) {
            return Ok(RestPolicy::PathGraph);
        }
        if [
            &self.policy.rest_global,
            &self.policy.rest_path_graph,
            &self.policy.rest_body_graph,
            &self.policy.rest_operation_graph,
            &self.policy.rest_job_graph,
            &self.policy.rest_profile_denied,
        ]
        .into_iter()
        .any(|entries| {
            entries.iter().any(|entry| {
                entry
                    .split_once(' ')
                    .is_some_and(|(_, path)| path == matched_path)
            })
        }) {
            // The path is classified, but Axum did not register this method.
            // Let its MethodRouter return the ordinary 405 without touching
            // state. Opaque `any` service proxies were denied above by path.
            return Ok(RestPolicy::Global);
        }
        Err(CellGraphBoundaryError::UnclassifiedRestRoute(key))
    }

    fn mcp_policy(&self, tool_name: &str) -> Result<McpPolicy, CellGraphBoundaryError> {
        if self.policy.mcp_profile_denied.contains(tool_name) {
            return Ok(McpPolicy::ProfileDenied);
        }
        if self.policy.mcp_job_graph.contains(tool_name) {
            return Ok(McpPolicy::JobGraph);
        }
        if self.policy.mcp_operation_guarded.contains(tool_name) {
            return Ok(McpPolicy::OperationGuarded);
        }
        if self.policy.mcp_global.contains(tool_name) {
            return Ok(McpPolicy::Global);
        }
        if self.mcp_graph_scoped.contains(tool_name) {
            return Ok(McpPolicy::GraphScoped);
        }
        Err(CellGraphBoundaryError::UnclassifiedMcpTool(
            tool_name.to_string(),
        ))
    }

    pub(crate) fn mcp_tool_visible(&self, tool_name: &str) -> bool {
        !self.is_enabled()
            || !matches!(
                self.mcp_policy(tool_name),
                Ok(McpPolicy::ProfileDenied) | Err(_)
            )
    }

    pub(crate) fn mcp_tool_visible_for_role(&self, tool_name: &str, role: CellRole) -> bool {
        if !self.is_enabled() {
            // The local/desktop loopback has no gateway-issued cell lease and
            // intentionally leaves the cell effect registry empty. Its bearer
            // token scopes remain authoritative; a nonexistent lease role must
            // not erase the entire MCP discovery surface.
            return true;
        }
        self.mcp_tool_visible(tool_name)
            && self
                .mcp_effects
                .get(tool_name)
                .and_then(minimum_role_for_effect)
                .is_some_and(|minimum| role >= minimum)
    }

    pub(crate) fn authorize_mcp_role(
        &self,
        tool_name: &str,
        role: CellRole,
    ) -> Result<(), CellGraphBoundaryError> {
        if !self.is_enabled() || self.mcp_tool_visible_for_role(tool_name, role) {
            Ok(())
        } else {
            Err(CellGraphBoundaryError::InsufficientRole(format!(
                "MCP tool {tool_name}"
            )))
        }
    }

    fn verify_cell_lease(&self, token: &str) -> Result<VerifiedCellLease, String> {
        use base64::Engine as _;
        use hmac::{Hmac, Mac};

        let secret = self
            .lease_secret
            .as_deref()
            .ok_or_else(|| "cell lease verification is not configured".to_string())?;
        let mut parts = token.split('.');
        let (Some(header), Some(payload), Some(signature), None) =
            (parts.next(), parts.next(), parts.next(), parts.next())
        else {
            return Err("malformed cell lease".into());
        };
        let header_json = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(header)
            .map_err(|_| "invalid cell lease header encoding".to_string())?;
        let header_value: serde_json::Value = serde_json::from_slice(&header_json)
            .map_err(|_| "invalid cell lease header".to_string())?;
        if header_value.get("alg").and_then(serde_json::Value::as_str) != Some("HS256") {
            return Err("cell lease algorithm must be HS256".into());
        }
        let signature = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(signature)
            .map_err(|_| "invalid cell lease signature encoding".to_string())?;
        let mut mac = Hmac::<Sha256>::new_from_slice(secret)
            .map_err(|_| "invalid cell lease verification secret".to_string())?;
        mac.update(format!("{header}.{payload}").as_bytes());
        mac.verify_slice(&signature)
            .map_err(|_| "invalid cell lease signature".to_string())?;
        let payload = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(payload)
            .map_err(|_| "invalid cell lease payload encoding".to_string())?;
        let claims: CellLeaseClaims = serde_json::from_slice(&payload)
            .map_err(|_| "invalid cell lease claims".to_string())?;
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|duration| duration.as_secs())
            .unwrap_or(0);
        let owner = self.owner_principal().ok_or("cell has no owner binding")?;
        let graph_id = self.owner_graph_id().ok_or("cell has no graph binding")?;
        let generation = self
            .graph_generation()
            .ok_or("cell has no generation binding")?;
        let registry_revision = self
            .registry_revision()
            .ok_or("cell has no revision binding")?;
        let expected_cell_id = bound_cell_id(owner, graph_id, generation);
        if claims.iss != "pn-gateway"
            || claims.aud != "gardend-cell"
            || claims.owner != owner
            || claims.graph_id != graph_id
            || claims.generation != generation
            || claims.cell_id != expected_cell_id
            || claims.registry_revision < registry_revision
            || claims.sub.trim().is_empty()
            || claims.session_id.trim().is_empty()
            || claims.policy_revision == 0
            || claims.iat > now.saturating_add(2)
            || claims.exp.saturating_add(2) < now
        {
            return Err("cell lease does not match this bound cell".into());
        }
        Ok(VerifiedCellLease {
            principal: claims.sub,
            role: claims.role,
            policy_revision: claims.policy_revision,
        })
    }

    pub(crate) fn authorize_json_graph_references(
        &self,
        value: &serde_json::Value,
    ) -> Result<(), CellGraphBoundaryError> {
        match self.owner_graph_id() {
            Some(owner_graph_id) => authorize_json_graph_selectors(owner_graph_id, value),
            None => Ok(()),
        }
    }

    /// Apply the same ownership rule to every durable CRDT operation, whether
    /// it arrived through REST/MCP, an internal helper, or journal recovery.
    /// This runs before any graph lease, graph directory, or Y.Doc is opened.
    pub(crate) fn authorize_crdt_operation(
        &self,
        kind: &str,
        graph_id: &str,
        payload: &serde_json::Value,
    ) -> Result<(), CellGraphBoundaryError> {
        if !self.is_enabled() {
            return Ok(());
        }
        authorize_crdt_operation_for_owner(
            self.owner_graph_id().expect("enabled cell has owner"),
            kind,
            graph_id,
            payload,
        )
    }

    fn authorize_rest_effect(
        &self,
        method: &Method,
        matched_path: &str,
        headers: &axum::http::HeaderMap,
        state: &LoopbackState,
        body: Option<&serde_json::Value>,
    ) -> Result<(), Response> {
        let method = if method == Method::HEAD {
            Method::GET
        } else {
            method.clone()
        };
        let key = format!("{method} {matched_path}");
        let Some(effect) = self.rest_effects.get(&key) else {
            // The only routes intentionally registered with `any` are the
            // opaque service proxies. They are denied in cell mode, but still
            // authenticate their authoritative proxy effect before revealing
            // that denial.
            if self
                .policy
                .rest_profile_denied_any_method
                .contains(matched_path)
            {
                return require_loopback_scopes(headers, state, &["services.proxy"]);
            }
            return Err(CellGraphBoundaryError::UnclassifiedRestRoute(key).http_response());
        };
        let scope_mode = effect.scope_mode.as_deref().unwrap_or("all");
        match scope_mode {
            "public" => Ok(()),
            "bearer-token" => {
                if loopback_master_token_presented(headers, state) {
                    Ok(())
                } else {
                    Err(loopback_error(
                        StatusCode::UNAUTHORIZED,
                        "missing or invalid loopback token (Authorization header or bearer.<token> subprotocol)",
                    ))
                }
            }
            "bearer-or-signed-url" => {
                if !origin_ok(headers) {
                    return Err(loopback_error(StatusCode::FORBIDDEN, "invalid origin"));
                }
                if bearer_token(headers).is_none() {
                    // Query-token validation is graph-local and remains in the
                    // handler. A foreign graph is mapped to the exact same
                    // unauthorized response as an invalid query token below.
                    Ok(())
                } else {
                    require_effect_scopes(headers, state, effect)
                }
            }
            "operation-kind" => {
                let kind = body
                    .and_then(serde_json::Value::as_object)
                    .and_then(|object| object.get("kind"))
                    .ok_or_else(|| {
                        CellGraphBoundaryError::InvalidRequest(
                            "CRDT operation kind is required".to_string(),
                        )
                        .http_response()
                    })?;
                let kind = kind
                    .as_str()
                    .map(str::trim)
                    .filter(|kind| !kind.is_empty())
                    .ok_or_else(|| {
                        CellGraphBoundaryError::InvalidRequest(
                            "CRDT operation kind must be a non-empty string".to_string(),
                        )
                        .http_response()
                    })?;
                let scopes = crdt_operation_scopes(kind).ok_or_else(|| {
                    CellGraphBoundaryError::InvalidRequest(format!(
                        "unsupported local CRDT operation kind {kind}"
                    ))
                    .http_response()
                })?;
                require_loopback_scopes(headers, state, &scopes)
            }
            "json-rpc-method" => Ok(()),
            "all" => require_effect_scopes(headers, state, effect),
            other => Err(CellGraphBoundaryError::InvalidRequest(format!(
                "unsupported REST scope mode {other}"
            ))
            .http_response()),
        }
    }

    pub(crate) fn authorize_job_id(
        &self,
        jobs: &crate::local_jobs::LocalJobRegistry,
        job_id: &str,
    ) -> Result<(), CellGraphBoundaryError> {
        if !self.is_enabled() {
            return Ok(());
        }
        if let Some(record) = jobs
            .get(job_id)
            .map_err(|error| CellGraphBoundaryError::InvalidRequest(error.message()))?
        {
            authorize_job_record(self, &record)?;
        } else {
            return Err(CellGraphBoundaryError::JobNotOwned);
        }
        Ok(())
    }

    pub(crate) fn filter_openapi(&self, openapi: &mut serde_json::Value) {
        if !self.is_enabled() {
            return;
        }
        let Some(paths) = openapi
            .get_mut("paths")
            .and_then(serde_json::Value::as_object_mut)
        else {
            return;
        };
        for (path, methods) in paths.iter_mut() {
            let Some(methods) = methods.as_object_mut() else {
                continue;
            };
            methods.retain(|method, _| {
                let key = format!("{} {path}", method.to_ascii_uppercase());
                matches!(
                    self.rest_policy(
                        &Method::from_bytes(method.as_bytes()).unwrap_or(Method::GET),
                        path,
                    ),
                    Ok(RestPolicy::Global)
                        | Ok(RestPolicy::PathGraph)
                        | Ok(RestPolicy::BodyGraph)
                        | Ok(RestPolicy::OperationGraph)
                        | Ok(RestPolicy::JobGraph)
                ) && !self.policy.rest_profile_denied.contains(&key)
            });
        }
        paths.retain(|_, methods| {
            methods
                .as_object()
                .is_some_and(|methods| !methods.is_empty())
        });
        openapi["x-sophia-cellGraphBoundary"] = serde_json::json!("single-graph-v1");
    }

    pub(crate) fn scope_mcp_arguments(
        &self,
        tool_name: &str,
        mut arguments: serde_json::Value,
        jobs: &crate::local_jobs::LocalJobRegistry,
    ) -> Result<serde_json::Value, CellGraphBoundaryError> {
        let Some(owner_graph_id) = self.owner_graph_id() else {
            return Ok(arguments);
        };
        match self.mcp_policy(tool_name)? {
            McpPolicy::Global => Ok(arguments),
            McpPolicy::ProfileDenied => Err(CellGraphBoundaryError::ProfileWideOperation(format!(
                "MCP tool {tool_name}"
            ))),
            McpPolicy::JobGraph => {
                // A declared graph selector is only a consistency assertion;
                // the durable job record below is the resource authority.
                // Validate both so a contradictory request fails closed while
                // never mistaking the supplied graph id for ownership proof.
                authorize_json_graph_selectors(owner_graph_id, &arguments)?;
                for job_id in required_job_carriers(&self.policy, tool_name, &arguments)? {
                    let record = jobs
                        .get(job_id)
                        .map_err(|error| CellGraphBoundaryError::InvalidRequest(error.message()))?
                        .ok_or(CellGraphBoundaryError::JobNotOwned)?;
                    authorize_job_record(self, &record)?;
                }
                Ok(arguments)
            }
            McpPolicy::OperationGuarded => {
                scope_graph_arguments(owner_graph_id, &mut arguments)?;
                authorize_mcp_operation(tool_name, owner_graph_id, &arguments)?;
                Ok(arguments)
            }
            McpPolicy::GraphScoped => {
                scope_graph_arguments(owner_graph_id, &mut arguments)?;
                Ok(arguments)
            }
        }
    }
}

/// JSON extractor for the closed set of routes whose graph selector lives in
/// the request body. It parses exactly once into `Value`, checks/injects the
/// cell owner, then moves the same allocations into `T` via `from_value`.
pub(crate) struct CellGraphJson<T>(pub(crate) T);

impl<S, T> FromRequest<S> for CellGraphJson<T>
where
    S: Send + Sync,
    Arc<LoopbackState>: FromRef<S>,
    T: DeserializeOwned,
{
    type Rejection = Response;

    async fn from_request(request: Request, state: &S) -> Result<Self, Self::Rejection> {
        let loopback_state = Arc::<LoopbackState>::from_ref(state);
        if !loopback_state.cell_graph.is_enabled() {
            return Json::<T>::from_request(request, state)
                .await
                .map(|Json(value)| CellGraphJson(value))
                .map_err(|rejection| rejection.into_response());
        }
        let method = request.method().clone();
        let headers = request.headers().clone();
        let matched_path = request
            .extensions()
            .get::<MatchedPath>()
            .map(|path| path.as_str().to_string())
            .ok_or_else(|| {
                CellGraphBoundaryError::InvalidRequest(
                    "graph-scoped request did not resolve a route".to_string(),
                )
                .http_response()
            })?;
        let Json(mut value) = Json::<serde_json::Value>::from_request(request, state)
            .await
            .map_err(|rejection| rejection.into_response())?;
        loopback_state.cell_graph.authorize_rest_effect(
            &method,
            &matched_path,
            &headers,
            &loopback_state,
            Some(&value),
        )?;
        let owner_graph_id = loopback_state
            .cell_graph
            .owner_graph_id()
            .expect("enabled cell has owner");
        scope_graph_arguments(owner_graph_id, &mut value).map_err(|error| error.http_response())?;
        if matches!(
            loopback_state
                .cell_graph
                .rest_policy(&method, &matched_path),
            Ok(RestPolicy::OperationGraph)
        ) {
            let object = value.as_object().expect("scoped graph JSON is an object");
            let kind = required_string(object, "kind", "CRDT operation kind")
                .map_err(|error| error.http_response())?;
            let graph_id =
                required_graph_selector(object).map_err(|error| error.http_response())?;
            let null_payload = serde_json::Value::Null;
            let payload = object.get("payload").unwrap_or(&null_payload);
            loopback_state
                .cell_graph
                .authorize_crdt_operation(kind, graph_id, payload)
                .map_err(|error| error.http_response())?;
        }
        serde_json::from_value(value)
            .map(CellGraphJson)
            .map_err(|error| loopback_error(StatusCode::UNPROCESSABLE_ENTITY, &error.to_string()))
    }
}

pub(crate) async fn cell_graph_boundary_middleware(
    State(state): State<Arc<LoopbackState>>,
    mut request: Request,
    next: Next,
) -> Response {
    if !state.cell_graph.is_enabled() {
        return run_with_cell_lease(request, next).await;
    }
    // CORS preflight never dispatches a graph operation. Keep it independent
    // of the method-specific ownership catalog even if layer ordering changes.
    if request.method() == Method::OPTIONS {
        return run_with_cell_lease(request, next).await;
    }
    let Some(matched_path) = request.extensions().get::<MatchedPath>() else {
        // A fallback/404 has no handler and therefore cannot touch graph
        // state. Preserve the router's ordinary not-found semantics.
        return run_with_cell_lease(request, next).await;
    };
    let matched_path = matched_path.as_str().to_string();
    let anonymous_signed_image = matched_path == "/artifacts/{graph_id}/images/{image_id}"
        && bearer_token(request.headers()).is_none();
    if matched_path != "/health" && !anonymous_signed_image {
        let verified = request
            .headers()
            .get("x-sophia-cell-lease")
            .and_then(|value| value.to_str().ok())
            .ok_or_else(|| "missing cell lease".to_string())
            .and_then(|token| state.cell_graph.verify_cell_lease(token));
        let verified = match verified {
            Ok(verified) => verified,
            Err(error) => {
                log::warn!(target: "cell_lease", "request rejected: {error}");
                return loopback_error(
                    StatusCode::UNAUTHORIZED,
                    "invalid cell authorization lease",
                );
            }
        };
        let method = if request.method() == Method::HEAD {
            Method::GET
        } else {
            request.method().clone()
        };
        let key = format!("{method} {matched_path}");
        if let Some(effect) = state.cell_graph.rest_effects.get(&key) {
            let minimum = minimum_role_for_effect(effect);
            if minimum.is_none_or(|minimum| verified.role < minimum) {
                return loopback_error(
                    StatusCode::FORBIDDEN,
                    "cell role does not permit this operation",
                );
            }
        }
        request.extensions_mut().insert(verified);
    }
    let policy = match state
        .cell_graph
        .rest_policy(request.method(), &matched_path)
    {
        Ok(policy) => policy,
        Err(error) => return error.http_response(),
    };
    match policy {
        RestPolicy::Global | RestPolicy::BodyGraph | RestPolicy::OperationGraph => {
            run_with_cell_lease(request, next).await
        }
        RestPolicy::ProfileDenied => {
            if let Err(response) = state.cell_graph.authorize_rest_effect(
                request.method(),
                &matched_path,
                request.headers(),
                &state,
                None,
            ) {
                return response;
            }
            CellGraphBoundaryError::ProfileWideOperation(format!(
                "{} {matched_path}",
                request.method()
            ))
            .http_response()
        }
        RestPolicy::PathGraph => {
            if let Err(response) = state.cell_graph.authorize_rest_effect(
                request.method(),
                &matched_path,
                request.headers(),
                &state,
                None,
            ) {
                return response;
            }
            let graph_id = match path_parameter(&matched_path, request.uri().path(), "{graph_id}") {
                Some(graph_id) => graph_id,
                None => {
                    return CellGraphBoundaryError::InvalidRequest(
                        "graph-scoped route did not resolve graph_id".to_string(),
                    )
                    .http_response()
                }
            };
            match state.cell_graph.authorize_graph_id(graph_id) {
                Ok(()) => run_with_cell_lease(request, next).await,
                Err(_) if anonymous_signed_image => loopback_error(
                    StatusCode::UNAUTHORIZED,
                    "missing or invalid image access token",
                ),
                Err(error) => error.http_response(),
            }
        }
        RestPolicy::JobGraph => {
            if let Err(response) = state.cell_graph.authorize_rest_effect(
                request.method(),
                &matched_path,
                request.headers(),
                &state,
                None,
            ) {
                return response;
            }
            if matched_path.contains("{graph_id}") {
                let Some(graph_id) =
                    path_parameter(&matched_path, request.uri().path(), "{graph_id}")
                else {
                    return CellGraphBoundaryError::InvalidRequest(
                        "job route did not resolve graph_id".to_string(),
                    )
                    .http_response();
                };
                if let Err(error) = state.cell_graph.authorize_graph_id(graph_id) {
                    return error.http_response();
                }
            }
            // Job records live in the profile job registry. Their handlers
            // call `authorize_job_id` after bearer/scope validation so an
            // unauthenticated request cannot turn the boundary into a job
            // existence oracle or force disk reads.
            run_with_cell_lease(request, next).await
        }
    }
}

async fn run_with_cell_lease(request: Request, next: Next) -> Response {
    match request.extensions().get::<VerifiedCellLease>().cloned() {
        Some(lease) => CURRENT_CELL_LEASE.scope(lease, next.run(request)).await,
        None => next.run(request).await,
    }
}

fn path_parameter<'a>(
    matched_path: &str,
    actual_path: &'a str,
    parameter: &str,
) -> Option<&'a str> {
    matched_path
        .trim_matches('/')
        .split('/')
        .zip(actual_path.trim_matches('/').split('/'))
        .find_map(|(template, actual)| (template == parameter).then_some(actual))
}

fn authorize_job_record(
    boundary: &CellGraphBoundary,
    record: &crate::local_jobs::LocalJobRecord,
) -> Result<(), CellGraphBoundaryError> {
    let matches = local_job_graph_id(record).as_deref() == boundary.owner_graph_id()
        && record.owner_principal.as_deref() == boundary.owner_principal()
        && record.graph_generation == boundary.graph_generation();
    matches
        .then_some(())
        .ok_or(CellGraphBoundaryError::JobNotOwned)
}

fn required_job_carriers<'a>(
    policy: &CellGraphBoundaryPolicy,
    tool_name: &str,
    arguments: &'a serde_json::Value,
) -> Result<Vec<&'a str>, CellGraphBoundaryError> {
    let object = arguments.as_object().ok_or_else(|| {
        CellGraphBoundaryError::InvalidRequest("MCP tool arguments must be an object".to_string())
    })?;
    let keys = policy
        .mcp_job_carriers
        .get(tool_name)
        .ok_or_else(|| CellGraphBoundaryError::UnclassifiedMcpTool(tool_name.to_string()))?;
    let mut selected: Option<(&str, &str)> = None;
    for key in keys {
        if let Some(value) = object.get(key) {
            let value = value
                .as_str()
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .ok_or_else(|| {
                    CellGraphBoundaryError::InvalidRequest(format!(
                        "resource carrier {key} must be a non-empty string"
                    ))
                })?;
            if let Some((selected_key, selected_value)) = selected {
                if value != selected_value {
                    return Err(CellGraphBoundaryError::InvalidRequest(format!(
                        "resource carrier aliases {selected_key} and {key} disagree"
                    )));
                }
            } else {
                selected = Some((key.as_str(), value));
            }
        }
    }
    let selected = selected.map(|(_, value)| value).ok_or_else(|| {
        CellGraphBoundaryError::InvalidRequest(format!("one of {} is required", keys.join(", ")))
    })?;
    // Validate every conventional durable-resource carrier present, even if
    // it is not part of this tool's preferred alias set. This prevents a
    // permissive downstream schema from smuggling a second, foreign job id.
    let mut carriers = vec![selected];
    for key in ["job_id", "jobId", "operation_id", "operationId"] {
        let Some(value) = object.get(key) else {
            continue;
        };
        let value = value
            .as_str()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .ok_or_else(|| {
                CellGraphBoundaryError::InvalidRequest(format!(
                    "resource carrier {key} must be a non-empty string"
                ))
            })?;
        if !carriers.contains(&value) {
            carriers.push(value);
        }
    }
    Ok(carriers)
}

fn authorize_json_graph_selectors(
    owner_graph_id: &str,
    value: &serde_json::Value,
) -> Result<(), CellGraphBoundaryError> {
    fn visit(
        owner_graph_id: &str,
        value: &serde_json::Value,
        depth: usize,
    ) -> Result<(), CellGraphBoundaryError> {
        if depth > 64 {
            return Err(CellGraphBoundaryError::InvalidRequest(
                "graph-scoped JSON nesting exceeds the cell boundary limit".to_string(),
            ));
        }
        match value {
            serde_json::Value::Object(object) => {
                for (key, nested) in object {
                    let normalized = key
                        .chars()
                        .filter(|character| character.is_ascii_alphanumeric())
                        .flat_map(char::to_lowercase)
                        .collect::<String>();
                    match normalized.as_str() {
                        "newgraphid" => {
                            require_selector_string(key, nested)?;
                            return Err(CellGraphBoundaryError::ProfileWideOperation(
                                "graph creation selector newGraphId".to_string(),
                            ));
                        }
                        "graphid" | "sourcegraphid" | "targetgraphid" | "scenegraphid" => {
                            let requested = require_selector_string(key, nested)?;
                            if requested != owner_graph_id {
                                return Err(CellGraphBoundaryError::GraphMismatch);
                            }
                        }
                        _ if normalized.ends_with("graphid")
                            || normalized.ends_with("graphids") =>
                        {
                            return Err(CellGraphBoundaryError::InvalidRequest(format!(
                                "unsupported graph selector {key}"
                            )));
                        }
                        _ => {}
                    }
                    visit(owner_graph_id, nested, depth + 1)?;
                }
            }
            serde_json::Value::Array(values) => {
                for nested in values {
                    visit(owner_graph_id, nested, depth + 1)?;
                }
            }
            _ => {}
        }
        Ok(())
    }
    visit(owner_graph_id, value, 0)
}

fn require_selector_string<'a>(
    key: &str,
    value: &'a serde_json::Value,
) -> Result<&'a str, CellGraphBoundaryError> {
    value
        .as_str()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| {
            CellGraphBoundaryError::InvalidRequest(format!(
                "graph selector {key} must be a non-empty string"
            ))
        })
}

fn scope_graph_arguments(
    owner_graph_id: &str,
    arguments: &mut serde_json::Value,
) -> Result<(), CellGraphBoundaryError> {
    authorize_json_graph_selectors(owner_graph_id, arguments)?;
    let object = arguments.as_object_mut().ok_or_else(|| {
        CellGraphBoundaryError::InvalidRequest(
            "graph-scoped request arguments must be an object".to_string(),
        )
    })?;
    if !object.contains_key("graphId") && !object.contains_key("graph_id") {
        object.insert(
            "graphId".to_string(),
            serde_json::Value::String(owner_graph_id.to_string()),
        );
    }
    Ok(())
}

fn required_string<'a>(
    object: &'a serde_json::Map<String, serde_json::Value>,
    key: &str,
    label: &str,
) -> Result<&'a str, CellGraphBoundaryError> {
    object
        .get(key)
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| {
            CellGraphBoundaryError::InvalidRequest(format!("{label} must be a non-empty string"))
        })
}

fn required_graph_selector(
    object: &serde_json::Map<String, serde_json::Value>,
) -> Result<&str, CellGraphBoundaryError> {
    for key in ["graphId", "graph_id"] {
        if let Some(value) = object.get(key) {
            return require_selector_string(key, value);
        }
    }
    Err(CellGraphBoundaryError::InvalidRequest(
        "graph selector is required".to_string(),
    ))
}

fn authorize_mcp_operation(
    tool_name: &str,
    owner_graph_id: &str,
    arguments: &serde_json::Value,
) -> Result<(), CellGraphBoundaryError> {
    let object = arguments.as_object().ok_or_else(|| {
        CellGraphBoundaryError::InvalidRequest("MCP tool arguments must be an object".to_string())
    })?;
    match tool_name {
        "delete" => {
            let delete_type = required_string(object, "type", "delete type")?.to_ascii_lowercase();
            if matches!(delete_type.as_str(), "graph" | "graphs") {
                return Err(CellGraphBoundaryError::ProfileWideOperation(
                    "MCP delete(type=graph)".to_string(),
                ));
            }
            Ok(())
        }
        "crdt_operation" => {
            let kind = required_string(object, "kind", "CRDT operation kind")?;
            let graph_id = required_graph_selector(object)?;
            let null_payload = serde_json::Value::Null;
            let payload = object.get("payload").unwrap_or(&null_payload);
            authorize_crdt_operation_for_owner(owner_graph_id, kind, graph_id, payload)
        }
        _ => Err(CellGraphBoundaryError::UnclassifiedMcpTool(
            tool_name.to_string(),
        )),
    }
}

fn authorize_crdt_operation_for_owner(
    owner_graph_id: &str,
    kind: &str,
    graph_id: &str,
    payload: &serde_json::Value,
) -> Result<(), CellGraphBoundaryError> {
    if graph_id.trim() != owner_graph_id {
        return Err(CellGraphBoundaryError::GraphMismatch);
    }
    if kind.trim() == "graph.importArchive" {
        return Err(CellGraphBoundaryError::ProfileWideOperation(
            "CRDT operation graph.importArchive".to_string(),
        ));
    }
    if kind.trim() == "graph.restoreArchive" {
        let object = payload.as_object().ok_or_else(|| {
            CellGraphBoundaryError::InvalidRequest(
                "graph.restoreArchive payload must be an object".to_string(),
            )
        })?;
        let new_graph_id = required_string(object, "newGraphId", "restore target graph")?;
        if new_graph_id != owner_graph_id {
            return Err(CellGraphBoundaryError::GraphMismatch);
        }
        for required in [
            "archiveSha256",
            "sourceGraphId",
            "sourceUserId",
            "planDigest",
        ] {
            required_string(object, required, required)?;
        }
        for required in [
            "targetGeneration",
            "expectedDocumentCount",
            "expectedRdfTripleCount",
        ] {
            if !object.get(required).is_some_and(serde_json::Value::is_u64) {
                return Err(CellGraphBoundaryError::InvalidRequest(format!(
                    "{required} must be a non-negative integer"
                )));
            }
        }
        let preservation_v2 = match object.get("formatVersion") {
            None => false,
            Some(value) if value.as_u64() == Some(2) => true,
            _ => return Err(CellGraphBoundaryError::InvalidRequest(
                "graph.restoreArchive unsupported explicit formatVersion".to_string(),
            )),
        };
        if object
            .get("includesArtifacts")
            .and_then(serde_json::Value::as_bool)
            != Some(preservation_v2)
        {
            return Err(CellGraphBoundaryError::InvalidRequest(
                "graph.restoreArchive artifacts require explicit preservation formatVersion 2".to_string(),
            ));
        }
        // Consume exactly the validated top-level target. Descendants and
        // aliases still pass through the closed, recursive selector policy.
        let mut remainder = object.clone();
        remainder.remove("newGraphId");
        return authorize_json_graph_selectors(owner_graph_id, &serde_json::Value::Object(remainder));
    }
    authorize_json_graph_selectors(owner_graph_id, payload)
}

fn require_effect_scopes(
    headers: &axum::http::HeaderMap,
    state: &LoopbackState,
    effect: &EffectEntry,
) -> Result<(), Response> {
    let required = effect.scopes.iter().map(String::as_str).collect::<Vec<_>>();
    require_loopback_scopes(headers, state, &required)
}

fn minimum_role_for_effect(effect: &EffectEntry) -> Option<CellRole> {
    let mut minimum = CellRole::Viewer;
    for scope in &effect.scopes {
        let role = match scope.as_str() {
            "artifacts.read"
            | "diagnostics.crdt-timings.read"
            | "documents.history.read"
            | "documents.read"
            | "entities.read"
            | "graphs.export"
            | "graphs.read"
            | "images.read"
            | "ingestion.config.read"
            | "jobs.read"
            | "loopback.manifest.read"
            | "loopback.openapi.read"
            | "mcp.tools.call"
            | "mcp.tools.read"
            | "memory.read"
            | "orientation.read"
            | "profile.read"
            | "rdf.dump"
            | "rdf.query"
            | "runtime.capabilities.read"
            | "salience.read"
            | "search.lexical.read"
            | "search.semantic.read"
            | "semantic.index.read"
            | "semantic.models.read"
            | "services.read"
            | "time-travel.read"
            | "wires.read"
            | "workspace.read" => CellRole::Viewer,
            "artifacts.delete"
            | "artifacts.ingest"
            | "artifacts.write"
            | "documents.delete.crdt"
            | "documents.snapshots.delete"
            | "documents.snapshots.write"
            | "documents.write.crdt"
            | "entities.delete"
            | "entities.write"
            | "graphs.delete"
            | "graphs.import"
            | "graphs.write"
            | "images.write"
            | "imports.web.write"
            | "ingestion.config.write"
            | "jobs.cancel"
            | "memory.write"
            | "rdf.load"
            | "rdf.update"
            | "salience.write"
            | "semantic.index.cancel"
            | "semantic.index.write"
            | "semantic.models.write"
            | "services.manage"
            | "services.proxy"
            | "time-travel.restore"
            | "time-travel.write"
            | "wires.delete"
            | "wires.write"
            | "workspace.delete.crdt"
            | "workspace.write.crdt" => CellRole::Editor,
            // source_pull also writes its ledger/checkpoint and repairs pending
            // effects and projections. Its sources.read scope is not a promise
            // of a read-only handler; viewers must not trigger those writes.
            "sources.read" | "sources.write" | "sources.rebuild" => CellRole::Editor,
            "graphs.restore" => CellRole::Owner,
            _ => return None,
        };
        minimum = minimum.max(role);
    }
    Some(minimum)
}

fn bound_cell_id(owner: &str, graph_id: &str, generation: u64) -> String {
    let digest = Sha256::digest(format!("{owner}\0{graph_id}\0{generation}").as_bytes());
    let mut hex = String::with_capacity(40);
    for byte in digest.iter().take(20) {
        use std::fmt::Write as _;
        let _ = write!(&mut hex, "{byte:02x}");
    }
    format!("c-{hex}")
}

fn validate_disjoint_policy_sets(policy: &CellGraphBoundaryPolicy) -> Result<(), String> {
    let rest_sets = [
        ("restGlobal", &policy.rest_global),
        ("restPathGraph", &policy.rest_path_graph),
        ("restBodyGraph", &policy.rest_body_graph),
        ("restOperationGraph", &policy.rest_operation_graph),
        ("restJobGraph", &policy.rest_job_graph),
        ("restProfileDenied", &policy.rest_profile_denied),
    ];
    for (index, (left_name, left)) in rest_sets.iter().enumerate() {
        for (right_name, right) in rest_sets.iter().skip(index + 1) {
            if let Some(overlap) = left.intersection(right).next() {
                return Err(format!(
                    "cell graph REST policy overlap between {left_name} and {right_name}: {overlap}"
                ));
            }
        }
    }
    let job_handler_operations = policy
        .rest_job_handlers
        .keys()
        .cloned()
        .collect::<BTreeSet<_>>();
    if job_handler_operations != policy.rest_job_graph {
        return Err("restJobHandlers keys must exactly match the restJobGraph policy".to_string());
    }
    let mcp_sets = [
        ("mcpGlobal", &policy.mcp_global),
        ("mcpJobGraph", &policy.mcp_job_graph),
        ("mcpOperationGuarded", &policy.mcp_operation_guarded),
        ("mcpProfileDenied", &policy.mcp_profile_denied),
        ("mcpGraphScoped", &policy.mcp_graph_scoped),
    ];
    for (index, (left_name, left)) in mcp_sets.iter().enumerate() {
        for (right_name, right) in mcp_sets.iter().skip(index + 1) {
            if let Some(overlap) = left.intersection(right).next() {
                return Err(format!(
                    "cell graph MCP policy overlap between {left_name} and {right_name}: {overlap}"
                ));
            }
        }
    }
    let carrier_tools = policy
        .mcp_job_carriers
        .keys()
        .cloned()
        .collect::<BTreeSet<_>>();
    if carrier_tools != policy.mcp_job_graph {
        return Err(
            "mcpJobCarriers keys must exactly match the mcpJobGraph policy set".to_string(),
        );
    }
    for (tool_name, keys) in &policy.mcp_job_carriers {
        if keys.is_empty() || keys.iter().any(|key| key.trim().is_empty()) {
            return Err(format!(
                "MCP job tool {tool_name} must declare non-empty resource carrier keys"
            ));
        }
    }
    Ok(())
}

fn validate_effect_registry(
    policy: &CellGraphBoundaryPolicy,
) -> Result<(BTreeMap<String, EffectEntry>, BTreeMap<String, EffectEntry>), String> {
    // Pin the authoritative artifact byte-for-byte. The original branch used
    // separate JS/Rust reconstructions of selected fields; those gates
    // disagreed on the same file and made every cell refuse to start. One raw
    // digest is language-neutral, while the structural checks below still
    // validate route/tool coverage and load-bearing effects semantically.
    let actual_registry_sha256 = format!("{:x}", Sha256::digest(EFFECT_REGISTRY_JSON.as_bytes()));
    if actual_registry_sha256 != policy.effect_registry_sha256 {
        return Err(format!(
            "loopback effect registry changed: policy pins {}, registry is {actual_registry_sha256}",
            policy.effect_registry_sha256
        ));
    }
    let (rest_effects, mcp_effects) = parse_effect_registry()?;

    for (operation, effect) in &rest_effects {
        let path = operation
            .split_once(' ')
            .map(|(_, path)| path)
            .unwrap_or_default();
        let has_service_effect = effect
            .scopes
            .iter()
            .any(|scope| matches!(scope.as_str(), "services.manage" | "services.proxy"));
        if has_service_effect
            && !policy.rest_profile_denied.contains(operation)
            && !policy.rest_profile_denied_any_method.contains(path)
        {
            return Err(format!(
                "REST operation {operation} has a child-service effect but is not denied in cell mode"
            ));
        }
        if effect.scopes.iter().any(|scope| scope == "graphs.delete")
            && !policy.rest_profile_denied.contains(operation)
        {
            return Err(format!(
                "REST operation {operation} can delete a graph but is not denied in cell mode"
            ));
        }
        if effect.scope_mode.as_deref() == Some("operation-kind")
            && !policy.rest_operation_graph.contains(operation)
        {
            return Err(format!(
                "REST operation {operation} has dynamic effects without an operation-aware cell guard"
            ));
        }
    }
    Ok((rest_effects, mcp_effects))
}

fn parse_effect_registry(
) -> Result<(BTreeMap<String, EffectEntry>, BTreeMap<String, EffectEntry>), String> {
    let registry: EffectRegistry = serde_json::from_str(EFFECT_REGISTRY_JSON)
        .map_err(|error| format!("parse embedded loopback effect registry: {error}"))?;
    let mut rest_effects = BTreeMap::new();
    for effect in registry.routes {
        let method = effect
            .method
            .as_deref()
            .ok_or_else(|| "REST effect is missing method".to_string())?;
        let path = effect
            .path
            .as_deref()
            .ok_or_else(|| "REST effect is missing path".to_string())?;
        let key = format!("{} {path}", method.to_ascii_uppercase());
        if rest_effects.insert(key.clone(), effect).is_some() {
            return Err(format!("duplicate REST effect entry {key}"));
        }
    }
    let mut mcp_effects = BTreeMap::new();
    for effect in registry.mcp_tools {
        let name = effect
            .name
            .clone()
            .ok_or_else(|| "MCP effect is missing name".to_string())?;
        if mcp_effects.insert(name.clone(), effect).is_some() {
            return Err(format!("duplicate MCP effect entry {name}"));
        }
    }

    Ok((rest_effects, mcp_effects))
}

fn known_catalog_tools() -> Result<BTreeSet<String>, String> {
    let catalog: serde_json::Value = serde_json::from_str(MCP_CATALOG_JSON)
        .map_err(|error| format!("parse embedded MCP tool catalog: {error}"))?;
    let tools = catalog
        .get("tools")
        .and_then(serde_json::Value::as_array)
        .ok_or_else(|| "embedded MCP tool catalog has no tools array".to_string())?;
    tools
        .iter()
        .map(|tool| {
            tool.get("name")
                .and_then(serde_json::Value::as_str)
                .map(str::to_string)
                .ok_or_else(|| "embedded MCP tool is missing a name".to_string())
        })
        .collect()
}

fn validate_and_classify_mcp_catalog(
    policy: &CellGraphBoundaryPolicy,
    effects: &BTreeMap<String, EffectEntry>,
) -> Result<BTreeSet<String>, String> {
    let catalog: serde_json::Value = serde_json::from_str(MCP_CATALOG_JSON)
        .map_err(|error| format!("parse embedded MCP tool catalog: {error}"))?;
    let tools = catalog
        .get("tools")
        .and_then(serde_json::Value::as_array)
        .ok_or_else(|| "embedded MCP tool catalog has no tools array".to_string())?;
    let mut catalog_names = BTreeSet::new();
    let mut graph_scoped = BTreeSet::new();
    for tool in tools {
        let name = tool
            .get("name")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| "embedded MCP tool is missing a name".to_string())?;
        if !catalog_names.insert(name.to_string()) {
            return Err(format!("duplicate MCP tool in embedded catalog: {name}"));
        }
        let effect = effects
            .get(name)
            .ok_or_else(|| format!("MCP tool {name} has no authoritative effect entry"))?;
        let has_service_effect = effect
            .scopes
            .iter()
            .any(|scope| matches!(scope.as_str(), "services.manage" | "services.proxy"));
        if has_service_effect && !policy.mcp_profile_denied.contains(name) {
            return Err(format!(
                "MCP tool {name} has a child-service effect but is not denied in cell mode"
            ));
        }
        if name == "upload_artifact" && !policy.mcp_profile_denied.contains(name) {
            return Err(
                "MCP upload_artifact reads arbitrary host paths and must be denied in cell mode"
                    .to_string(),
            );
        }
        if effect.scopes.iter().any(|scope| scope == "graphs.delete")
            && !policy.mcp_operation_guarded.contains(name)
            && !policy.mcp_profile_denied.contains(name)
        {
            return Err(format!(
                "MCP tool {name} can delete graphs without an operation-aware cell guard"
            ));
        }
        if effect.scopes.iter().any(|scope| scope == "graphs.import")
            && !policy.mcp_operation_guarded.contains(name)
            && !policy.mcp_profile_denied.contains(name)
        {
            return Err(format!(
                "MCP tool {name} can import graphs without an operation-aware cell guard"
            ));
        }
        let classes = [
            policy.mcp_global.contains(name),
            policy.mcp_job_graph.contains(name),
            policy.mcp_operation_guarded.contains(name),
            policy.mcp_profile_denied.contains(name),
            policy.mcp_graph_scoped.contains(name),
        ]
        .into_iter()
        .filter(|classified| *classified)
        .count();
        if classes != 1 {
            return Err(format!(
                "MCP tool {name} must have exactly one explicit cell-boundary policy (found {classes})"
            ));
        }
        if !policy.mcp_graph_scoped.contains(name) {
            continue;
        }
        let properties = tool
            .pointer("/inputSchema/properties")
            .and_then(serde_json::Value::as_object)
            .ok_or_else(|| format!("MCP tool {name} has no inputSchema properties"))?;
        if !properties.contains_key("graphId") && !properties.contains_key("graph_id") {
            return Err(format!(
                "MCP tool {name} is unclassified by the cell graph boundary"
            ));
        }
        graph_scoped.insert(name.to_string());
    }
    for classified in policy
        .mcp_global
        .iter()
        .chain(policy.mcp_job_graph.iter())
        .chain(policy.mcp_operation_guarded.iter())
        .chain(policy.mcp_profile_denied.iter())
        .chain(policy.mcp_graph_scoped.iter())
    {
        if !catalog_names.contains(classified) {
            return Err(format!(
                "cell graph boundary classifies unknown MCP tool {classified}"
            ));
        }
    }
    let actual_sha256 = sha256_lines(catalog_names.iter().map(String::as_str));
    if actual_sha256 != policy.mcp_surface_sha256 {
        return Err(format!(
            "MCP surface changed: policy pins {}, catalog is {actual_sha256}",
            policy.mcp_surface_sha256
        ));
    }
    let effect_names = effects.keys().cloned().collect::<BTreeSet<_>>();
    if effect_names != catalog_names {
        return Err("MCP effect registry and catalog tool names differ".to_string());
    }
    Ok(graph_scoped)
}

fn validate_rest_openapi_surface(
    policy: &CellGraphBoundaryPolicy,
    effects: &BTreeMap<String, EffectEntry>,
) -> Result<(), String> {
    let openapi: serde_json::Value =
        serde_json::from_str(crate::runtime_config::LOCAL_OPENAPI_JSON)
            .map_err(|error| format!("parse embedded local OpenAPI: {error}"))?;
    let paths = openapi
        .get("paths")
        .and_then(serde_json::Value::as_object)
        .ok_or_else(|| "embedded local OpenAPI has no paths object".to_string())?;
    let mut operations = Vec::new();
    for (path, methods) in paths {
        let methods = methods
            .as_object()
            .ok_or_else(|| format!("local OpenAPI path {path} is not an object"))?;
        for method in methods.keys() {
            operations.push(format!("{} {path}", method.to_ascii_uppercase()));
        }
    }
    operations.sort();
    let actual_sha256 = sha256_lines(operations.iter().map(String::as_str));
    if actual_sha256 != policy.rest_surface_sha256 {
        return Err(format!(
            "REST surface changed: policy pins {}, OpenAPI is {actual_sha256}",
            policy.rest_surface_sha256
        ));
    }
    let operations = operations.into_iter().collect::<BTreeSet<_>>();
    let effect_operations = effects.keys().cloned().collect::<BTreeSet<_>>();
    if operations != effect_operations {
        return Err("REST effect registry and OpenAPI operations differ".to_string());
    }
    Ok(())
}

fn sha256_lines<'a>(lines: impl IntoIterator<Item = &'a str>) -> String {
    let mut hasher = Sha256::new();
    for line in lines {
        hasher.update(line.as_bytes());
        hasher.update(b"\n");
    }
    format!("{:x}", hasher.finalize())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::local_jobs::LocalJobRegistry;
    use base64::Engine as _;
    use hmac::{Hmac, Mac};
    use std::fs;
    use std::time::{SystemTime, UNIX_EPOCH};
    use uuid::Uuid;

    fn sign_test_lease(secret: &[u8], claims: &serde_json::Value) -> String {
        let header = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(br#"{"alg":"HS256","typ":"JWT"}"#);
        let payload = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(serde_json::to_vec(claims).unwrap());
        let signing_input = format!("{header}.{payload}");
        let mut mac = Hmac::<Sha256>::new_from_slice(secret).unwrap();
        mac.update(signing_input.as_bytes());
        let signature =
            base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(mac.finalize().into_bytes());
        format!("{signing_input}.{signature}")
    }

    #[test]
    fn desktop_mode_is_noop_and_cell_mode_hides_foreign_graphs() {
        let desktop = CellGraphBoundary::for_test(None);
        assert!(desktop.authorize_graph_id("graph-b").is_ok());
        assert!(desktop.mcp_tool_visible_for_role("get_workspace", CellRole::Owner));
        assert!(desktop.mcp_tool_visible_for_role("create_document", CellRole::Viewer));

        let cell = CellGraphBoundary::for_test(Some("graph-a"));
        assert!(cell.authorize_graph_id("graph-a").is_ok());
        assert_eq!(
            cell.authorize_graph_id("graph-b"),
            Err(CellGraphBoundaryError::GraphMismatch)
        );
        #[cfg(all(feature = "headless", not(feature = "desktop")))]
        {
            assert!(cell.authorize_graph_self_heal("graph-a").is_ok());
            assert_eq!(
                cell.authorize_graph_self_heal("graph-b"),
                Err("graph not found: graph-b".to_string())
            );
        }
        assert!(CellGraphBoundary::new(Some(String::new())).is_err());
    }

    #[test]
    fn artifact_text_signed_roles_and_exact_graph_authority() {
        let secret=b"html-lease-test-secret-at-least-32-bytes".to_vec();
        let cell=CellGraphBoundary::new_with_binding(Some("graph-a".into()),Some("user:owner".into()),Some(3),Some(9),Some(secret.clone())).unwrap();
        let now=SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs();
        for role in [CellRole::Viewer,CellRole::Editor,CellRole::Owner] {
            let claims=serde_json::json!({"iss":"pn-gateway","aud":"gardend-cell","sub":"user:html","owner":"user:owner","graphId":"graph-a","generation":3,"cellId":bound_cell_id("user:owner","graph-a",3),"role":role,"policyRevision":7,"registryRevision":9,"sessionId":"html-native-test","iat":now,"exp":now+60});
            let verified=cell.verify_cell_lease(&sign_test_lease(&secret,&claims)).unwrap();
            assert!(cell.authorize_mcp_role("read_artifact",verified.role).is_ok());
            for tool in ["create_artifact_text","write_artifact_text"] {assert_eq!(cell.authorize_mcp_role(tool,verified.role).is_ok(),role>=CellRole::Editor);}
            let mut wrong=claims.clone();wrong["graphId"]=serde_json::json!("foreign");
            assert!(cell.verify_cell_lease(&sign_test_lease(&secret,&wrong)).is_err());
        }
        assert!(matches!(cell.rest_policy(&Method::GET,"/artifacts/{graph_id}/{artifact_id}/text"),Ok(RestPolicy::PathGraph)));
        assert!(cell.authorize_crdt_operation("artifact.mutateText","foreign",&serde_json::json!({})).is_err());
    }

    #[test]
    fn agent_status_signed_role_and_graph_refusals() {
        let secret=b"status-synthetic-lease-secret-at-least-32".to_vec();
        let cell=CellGraphBoundary::new_with_binding(Some("graph-a".into()),Some("user:owner".into()),Some(3),Some(9),Some(secret.clone())).unwrap();
        let now=SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs();
        for role in [CellRole::Viewer,CellRole::Editor,CellRole::Owner] {
            let claims=serde_json::json!({"iss":"pn-gateway","aud":"gardend-cell","sub":"user:status","owner":"user:owner","graphId":"graph-a","generation":3,"cellId":bound_cell_id("user:owner","graph-a",3),"role":role,"policyRevision":7,"registryRevision":9,"sessionId":"status-native-test","iat":now,"exp":now+60});
            let verified=cell.verify_cell_lease(&sign_test_lease(&secret,&claims)).unwrap();
            assert_eq!(verified.principal,"user:status");
            assert!(cell.authorize_mcp_role("agent_status",verified.role).is_ok());
            assert_eq!(cell.authorize_mcp_role("status",verified.role).is_ok(),role>=CellRole::Editor);
            assert_eq!(cell.mcp_tool_visible_for_role("status",verified.role),role>=CellRole::Editor);
            let mut foreign=claims.clone(); foreign["graphId"]=serde_json::json!("foreign");
            assert!(cell.verify_cell_lease(&sign_test_lease(&secret,&foreign)).is_err());
        }
        let jobs=LocalJobRegistry::new(std::env::temp_dir().join(format!("garden-status-jobs-{}",uuid::Uuid::new_v4()))).unwrap();
        assert!(cell.scope_mcp_arguments("status",serde_json::json!({"graphId":"foreign"}),&jobs).is_err());
        assert!(cell.scope_mcp_arguments("agent_status",serde_json::json!({"graphId":"graph-a","graph_id":"foreign"}),&jobs).is_err());
    }

    #[test]
    fn signed_cell_lease_is_exactly_bound_and_roles_filter_discovery() {
        let secret = b"test-cell-lease-secret-at-least-32-bytes".to_vec();
        let cell = CellGraphBoundary::new_with_binding(
            Some("graph-a".to_string()),
            Some("user:test-owner".to_string()),
            Some(3),
            Some(9),
            Some(secret.clone()),
        )
        .unwrap();
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let claims = serde_json::json!({
            "iss": "pn-gateway",
            "aud": "gardend-cell",
            "sub": "user:reader",
            "owner": "user:test-owner",
            "graphId": "graph-a",
            "generation": 3,
            "cellId": bound_cell_id("user:test-owner", "graph-a", 3),
            "role": "viewer",
            "policyRevision": 7,
            "registryRevision": 9,
            "sessionId": "session-1",
            "iat": now,
            "exp": now + 60,
        });
        let verified = cell
            .verify_cell_lease(&sign_test_lease(&secret, &claims))
            .unwrap();
        assert_eq!(verified.principal, "user:reader");
        assert_eq!(verified.role, CellRole::Viewer);
        assert!(cell.mcp_tool_visible_for_role("search_documents", CellRole::Viewer));
        assert!(!cell.mcp_tool_visible_for_role("create_document", CellRole::Viewer));
        assert!(cell.mcp_tool_visible_for_role("create_document", CellRole::Editor));
        assert_eq!(
            cell.authorize_mcp_role("create_document", CellRole::Viewer),
            Err(CellGraphBoundaryError::InsufficientRole(
                "MCP tool create_document".to_string()
            ))
        );

        for (field, wrong) in [
            ("owner", serde_json::json!("user:other")),
            ("graphId", serde_json::json!("graph-b")),
            ("generation", serde_json::json!(4)),
            ("registryRevision", serde_json::json!(8)),
            ("cellId", serde_json::json!("c-wrong")),
            ("exp", serde_json::json!(now.saturating_sub(10))),
        ] {
            let mut poisoned = claims.clone();
            poisoned[field] = wrong;
            assert!(
                cell.verify_cell_lease(&sign_test_lease(&secret, &poisoned))
                    .is_err(),
                "poisoned {field} lease must fail closed"
            );
        }
        let mut token = sign_test_lease(&secret, &claims);
        token.push('x');
        assert!(cell.verify_cell_lease(&token).is_err());
    }

    #[test]
    fn source_tools_allow_signed_editors_and_owners_but_not_viewers() {
        let secret = b"source-role-test-secret-at-least-32-bytes".to_vec();
        let cell = CellGraphBoundary::new_with_binding(
            Some("graph-a".to_string()),
            Some("user:test-owner".to_string()),
            Some(3),
            Some(9),
            Some(secret.clone()),
        )
        .unwrap();
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();
        for role in [CellRole::Viewer, CellRole::Editor, CellRole::Owner] {
            let claims = serde_json::json!({
                "iss": "pn-gateway",
                "aud": "gardend-cell",
                "sub": "user:source-caller",
                "owner": "user:test-owner",
                "graphId": "graph-a",
                "generation": 3,
                "cellId": bound_cell_id("user:test-owner", "graph-a", 3),
                "role": role,
                "policyRevision": 7,
                "registryRevision": 9,
                "sessionId": "source-session",
                "iat": now,
                "exp": now + 60,
            });
            let verified = cell
                .verify_cell_lease(&sign_test_lease(&secret, &claims))
                .unwrap();
            assert_eq!(verified.role, role);
            for (tool, scope) in [
                ("source_pull", "sources.read"),
                ("source_push", "sources.write"),
                ("source_rebuild", "sources.rebuild"),
            ] {
                // Use the embedded effect registry and the same discovery and
                // dispatch gates as hosted MCP, not a separate role table.
                let effect = cell.mcp_effects.get(tool).expect("registered source tool");
                assert!(effect.scopes.iter().any(|value| value == scope));
                assert_eq!(cell.mcp_policy(tool), Ok(McpPolicy::GraphScoped));
                assert_eq!(minimum_role_for_effect(effect), Some(CellRole::Editor));
                let permitted = role >= CellRole::Editor;
                assert_eq!(cell.mcp_tool_visible_for_role(tool, verified.role), permitted);
                assert_eq!(cell.authorize_mcp_role(tool, verified.role).is_ok(), permitted);
            }

            // Making sources reachable must not turn a valid signature for
            // another graph/incarnation into authority over this cell.
            for (field, wrong) in [
                ("graphId", serde_json::json!("graph-b")),
                ("generation", serde_json::json!(4)),
                ("owner", serde_json::json!("user:other")),
            ] {
                let mut foreign = claims.clone();
                foreign[field] = wrong;
                assert!(cell
                    .verify_cell_lease(&sign_test_lease(&secret, &foreign))
                    .is_err());
            }
        }
    }

    #[test]
    fn source_tools_still_fail_closed_for_an_unclassified_effect() {
        let mut cell = CellGraphBoundary::for_test(Some("graph-a"));
        Arc::make_mut(&mut cell.mcp_effects)
            .get_mut("source_pull")
            .expect("registered source pull")
            .scopes
            .push("sources.future".to_string());
        for role in [CellRole::Viewer, CellRole::Editor, CellRole::Owner] {
            assert!(!cell.mcp_tool_visible_for_role("source_pull", role));
            assert!(matches!(
                cell.authorize_mcp_role("source_pull", role),
                Err(CellGraphBoundaryError::InsufficientRole(_))
            ));
        }
    }

    #[test]
    fn create_once_signed_roles_graph_binding_and_closed_surface_negatives() {
        let secret = b"create-once-role-test-secret-at-least-32-bytes".to_vec();
        let cell = CellGraphBoundary::new_with_binding(Some("graph-a".into()), Some("user:test-owner".into()), Some(3), Some(9), Some(secret.clone())).unwrap();
        assert!(cell.surface_drift.is_empty());
        let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs();
        for role in [CellRole::Viewer, CellRole::Editor, CellRole::Owner] {
            let claims = serde_json::json!({
                "iss":"pn-gateway", "aud":"gardend-cell", "sub":"user:caller", "owner":"user:test-owner",
                "graphId":"graph-a", "generation":3, "cellId":bound_cell_id("user:test-owner","graph-a",3),
                "role":role, "policyRevision":7, "registryRevision":9, "sessionId":"create-once-test", "iat":now, "exp":now+60,
            });
            let verified = cell.verify_cell_lease(&sign_test_lease(&secret,&claims)).unwrap();
            assert_eq!(cell.authorize_mcp_role("create_document_once",verified.role).is_ok(), role >= CellRole::Editor);
            assert_eq!(cell.mcp_tool_visible_for_role("create_document_once",verified.role), role >= CellRole::Editor);
            let mut foreign = claims.clone(); foreign["graphId"] = serde_json::json!("graph-b");
            assert!(cell.verify_cell_lease(&sign_test_lease(&secret,&foreign)).is_err());
        }
        let jobs_dir = std::env::temp_dir().join(format!("garden-create-once-boundary-{}", uuid::Uuid::new_v4()));
        let jobs = crate::local_jobs::LocalJobRegistry::new(jobs_dir.clone()).unwrap();
        assert!(cell.scope_mcp_arguments("create_document_once",serde_json::json!({"graph_id":"graph-b"}),&jobs).is_err());
        assert!(cell.authorize_crdt_operation("document.createOnce","graph-b",&serde_json::json!({})).is_err());
        assert!(cell.authorize_mcp_role("create_document_once_future",CellRole::Owner).is_err());
        assert!(crate::loopback_scopes::crdt_operation_scopes("document.createOnceFuture").is_none());
        let mut policy: CellGraphBoundaryPolicy = serde_json::from_str(POLICY_JSON).unwrap();
        policy.effect_registry_sha256 = "stale".into();
        assert!(validate_effect_registry(&policy).is_err());
        let mut policy: CellGraphBoundaryPolicy = serde_json::from_str(POLICY_JSON).unwrap();
        policy.mcp_surface_sha256 = "stale".into();
        assert!(validate_and_classify_mcp_catalog(&policy,&cell.mcp_effects).is_err());
        let mut unclassified = cell.clone();
        Arc::make_mut(&mut unclassified.mcp_effects).get_mut("create_document_once").unwrap().scopes.push("future.unknown".into());
        assert!(unclassified.authorize_mcp_role("create_document_once",CellRole::Owner).is_err());
        drop(jobs);
        let _ = std::fs::remove_dir_all(jobs_dir);
    }

    #[test]
    fn route_policy_is_closed_and_lifecycle_routes_are_denied() {
        let cell = CellGraphBoundary::for_test(Some("graph-a"));
        assert_eq!(
            cell.rest_policy(&Method::GET, "/documents/{graph_id}"),
            Ok(RestPolicy::PathGraph)
        );
        assert_eq!(
            cell.rest_policy(&Method::POST, "/api/sparql/query"),
            Ok(RestPolicy::BodyGraph)
        );
        assert_eq!(
            cell.rest_policy(&Method::POST, "/api/crdt/operations"),
            Ok(RestPolicy::OperationGraph)
        );
        assert_eq!(
            cell.rest_policy(&Method::GET, "/health"),
            Ok(RestPolicy::Global)
        );
        assert_eq!(
            cell.rest_policy(&Method::DELETE, "/graphs/{graph_id}"),
            Ok(RestPolicy::ProfileDenied)
        );
        assert_eq!(
            cell.rest_policy(&Method::PATCH, "/graphs"),
            Ok(RestPolicy::Global)
        );
        assert!(matches!(
            cell.rest_policy(&Method::GET, "/new-profile-wide-route"),
            Err(CellGraphBoundaryError::UnclassifiedRestRoute(_))
        ));
    }

    #[test]
    fn boundary_error_statuses_are_stable_and_owner_free() {
        let mismatch = CellGraphBoundaryError::GraphMismatch.http_response();
        assert_eq!(mismatch.status(), StatusCode::NOT_FOUND);
        assert!(!CellGraphBoundaryError::GraphMismatch
            .mcp_message()
            .contains("graph-a"));
        assert_eq!(CellGraphBoundaryError::GraphMismatch.mcp_code(), -32004);
        assert_eq!(
            CellGraphBoundaryError::InvalidRequest("bad".to_string()).mcp_code(),
            -32602
        );

        let denied =
            CellGraphBoundaryError::ProfileWideOperation("GET /graphs".to_string()).http_response();
        assert_eq!(denied.status(), StatusCode::FORBIDDEN);

        let unclassified =
            CellGraphBoundaryError::UnclassifiedRestRoute("GET /new".to_string()).http_response();
        assert_eq!(unclassified.status(), StatusCode::SERVICE_UNAVAILABLE);
    }

    #[test]
    fn path_parameter_extracts_graph_and_job_without_decoding_data() {
        assert_eq!(
            path_parameter(
                "/navigation/{graph_id}/job-result/{job_id}",
                "/navigation/graph-a/job-result/job-1",
                "{graph_id}",
            ),
            Some("graph-a")
        );
        assert_eq!(
            path_parameter(
                "/navigation/{graph_id}/job-result/{job_id}",
                "/navigation/graph-a/job-result/job-1",
                "{job_id}",
            ),
            Some("job-1")
        );
    }

    #[test]
    fn cell_openapi_hides_disabled_operations_without_changing_desktop() {
        let source = serde_json::json!({
            "paths": {
                "/graphs": {"get": {}, "post": {}},
                "/graphs/{graph_id}": {"get": {}, "put": {}, "delete": {}},
                "/documents/{graph_id}": {"get": {}},
            }
        });
        let mut desktop_openapi = source.clone();
        CellGraphBoundary::for_test(None).filter_openapi(&mut desktop_openapi);
        assert_eq!(desktop_openapi, source);

        let mut cell_openapi = source;
        CellGraphBoundary::for_test(Some("graph-a")).filter_openapi(&mut cell_openapi);
        assert!(cell_openapi["paths"].get("/graphs").is_none());
        assert!(cell_openapi["paths"]["/graphs/{graph_id}"]
            .get("delete")
            .is_none());
        assert!(cell_openapi["paths"]["/graphs/{graph_id}"]
            .get("get")
            .is_some());
        assert_eq!(
            cell_openapi["x-sophia-cellGraphBoundary"],
            "single-graph-v1"
        );
    }

    #[test]
    fn mcp_scope_injects_owner_and_rejects_alias_and_wire_target_mismatches() {
        let jobs_dir =
            std::env::temp_dir().join(format!("garden-cell-boundary-jobs-{}", Uuid::new_v4()));
        let jobs = LocalJobRegistry::new(jobs_dir.clone()).expect("job registry");
        let cell = CellGraphBoundary::for_test(Some("graph-a"));

        for tool in ["read_document", "source_pull", "source_push", "source_rebuild"] {
            assert_eq!(
                cell.scope_mcp_arguments(tool, serde_json::json!({}), &jobs)
                    .unwrap()
                    .get("graphId"),
                Some(&serde_json::json!("graph-a"))
            );
            assert_eq!(
                cell.scope_mcp_arguments(
                    tool,
                    serde_json::json!({"graph_id": "graph-b"}),
                    &jobs,
                ),
                Err(CellGraphBoundaryError::GraphMismatch)
            );
        }
        assert_eq!(
            cell.scope_mcp_arguments(
                "create_wires",
                serde_json::json!({
                    "graphId": "graph-a",
                    "wires": [{"targetGraphId": "graph-b"}],
                }),
                &jobs,
            ),
            Err(CellGraphBoundaryError::GraphMismatch)
        );
        assert!(matches!(
            cell.scope_mcp_arguments("list_graphs", serde_json::json!({}), &jobs),
            Err(CellGraphBoundaryError::ProfileWideOperation(_))
        ));

        let valid_restore = serde_json::json!({
            "newGraphId": "graph-a",
            "archiveSha256": "a".repeat(64),
            "sourceGraphId": "graph-a",
            "sourceUserId": "test-owner",
            "targetGeneration": 1,
            "planDigest": "b".repeat(64),
            "expectedDocumentCount": 1,
            "expectedRdfTripleCount": 2,
            "includesArtifacts": false,
        });
        assert!(cell
            .authorize_crdt_operation("graph.restoreArchive", "graph-a", &valid_restore)
            .is_ok());
        let mut foreign_restore = valid_restore.clone();
        foreign_restore["newGraphId"] = serde_json::json!("graph-b");
        assert_eq!(
            cell.authorize_crdt_operation("graph.restoreArchive", "graph-a", &foreign_restore,),
            Err(CellGraphBoundaryError::GraphMismatch)
        );
        let mut artifact_restore = valid_restore;
        artifact_restore["includesArtifacts"] = serde_json::json!(true);
        assert!(matches!(
            cell.authorize_crdt_operation("graph.restoreArchive", "graph-a", &artifact_restore,),
            Err(CellGraphBoundaryError::InvalidRequest(_))
        ));

        let _ = fs::remove_dir_all(jobs_dir);
    }

    #[test]
    fn recursive_selectors_fail_closed_without_desktop_regression() {
        let desktop = CellGraphBoundary::for_test(None);
        assert!(desktop
            .authorize_json_graph_references(&serde_json::json!({
                "graphId": 7,
                "payload": {"destinationGraphId": "graph-b"},
            }))
            .is_ok());

        let cell = CellGraphBoundary::for_test(Some("graph-a"));
        assert!(cell
            .authorize_json_graph_references(&serde_json::json!({
                "payload": [{"nested": {"scene_graph_id": "graph-a"}}],
            }))
            .is_ok());
        assert!(matches!(
            cell.authorize_json_graph_references(&serde_json::json!({"graphId": 7})),
            Err(CellGraphBoundaryError::InvalidRequest(_))
        ));
        assert_eq!(
            cell.authorize_json_graph_references(&serde_json::json!({
                "payload": [{"nested": {"sceneGraphId": "graph-b"}}],
            })),
            Err(CellGraphBoundaryError::GraphMismatch)
        );
        assert!(matches!(
            cell.authorize_json_graph_references(&serde_json::json!({
                "payload": {"newGraphId": "graph-a"},
            })),
            Err(CellGraphBoundaryError::ProfileWideOperation(_))
        ));
        assert!(matches!(
            cell.authorize_json_graph_references(&serde_json::json!({
                "destinationGraphId": "graph-a",
            })),
            Err(CellGraphBoundaryError::InvalidRequest(_))
        ));
    }

    #[test]
    fn operation_effects_cannot_delete_import_read_host_files_or_spawn_services() {
        let jobs_dir =
            std::env::temp_dir().join(format!("garden-cell-boundary-jobs-{}", Uuid::new_v4()));
        let jobs = LocalJobRegistry::new(jobs_dir.clone()).expect("job registry");
        let cell = CellGraphBoundary::for_test(Some("graph-a"));

        assert!(matches!(
            cell.scope_mcp_arguments(
                "delete",
                serde_json::json!({"graphId": "graph-a", "type": "graph"}),
                &jobs,
            ),
            Err(CellGraphBoundaryError::ProfileWideOperation(_))
        ));
        assert!(cell
            .scope_mcp_arguments(
                "delete",
                serde_json::json!({
                    "graphId": "graph-a",
                    "type": "documents",
                    "documentId": "doc-a",
                }),
                &jobs,
            )
            .is_ok());
        assert!(matches!(
            cell.scope_mcp_arguments(
                "crdt_operation",
                serde_json::json!({
                    "graphId": "graph-a",
                    "kind": "graph.importArchive",
                    "payload": {"newGraphId": "graph-b"},
                }),
                &jobs,
            ),
            Err(CellGraphBoundaryError::ProfileWideOperation(_))
        ));
        for denied in [
            "upload_artifact",
            "workflow_authoring_session",
            "workflow_run_start",
            "workflow_run_monitor",
            "graph_intuition",
        ] {
            assert!(matches!(
                cell.scope_mcp_arguments(
                    denied,
                    serde_json::json!({
                        "graphId": "graph-a",
                        "file_path": "/etc/passwd",
                    }),
                    &jobs,
                ),
                Err(CellGraphBoundaryError::ProfileWideOperation(_))
            ));
        }
        assert_eq!(
            cell.authorize_crdt_operation("document.write", "graph-b", &serde_json::json!({}),),
            Err(CellGraphBoundaryError::GraphMismatch)
        );
        assert!(matches!(
            cell.authorize_crdt_operation(
                "graph.importArchive",
                "graph-a",
                &serde_json::json!({"newGraphId": "graph-a"}),
            ),
            Err(CellGraphBoundaryError::ProfileWideOperation(_))
        ));

        let _ = fs::remove_dir_all(jobs_dir);
    }

    #[tokio::test]
    async fn job_tools_require_owned_generation_bound_submission_metadata() {
        let jobs_dir =
            std::env::temp_dir().join(format!("garden-cell-boundary-jobs-{}", Uuid::new_v4()));
        let cell = CellGraphBoundary::for_test(Some("graph-a"));
        let jobs = LocalJobRegistry::new_bound(jobs_dir.clone(), &cell).expect("job registry");
        let lease = VerifiedCellLease {
            principal: "user:reader".to_string(),
            role: CellRole::Editor,
            policy_revision: 7,
        };
        let owned = CURRENT_CELL_LEASE
            .scope(lease.clone(), async {
                jobs.insert_queued("test", Some("graph-a".to_string()), serde_json::json!({}))
            })
            .await
            .unwrap();
        assert!(CURRENT_CELL_LEASE
            .scope(lease, async {
                jobs.insert_queued("test", Some("graph-b".to_string()), serde_json::json!({}))
            })
            .await
            .is_err());

        assert!(cell
            .scope_mcp_arguments(
                "get_job_status",
                serde_json::json!({"jobId": owned.job_id}),
                &jobs,
            )
            .is_ok());
        let mut stale = owned.clone();
        stale.graph_generation = Some(0);
        let mut foreign = owned.clone();
        foreign.job_id = "job-from-another-generation".to_string();
        foreign.graph_generation = Some(owned.graph_generation.unwrap_or(1) + 1);
        assert_eq!(
            authorize_job_record(&cell, &stale),
            Err(CellGraphBoundaryError::JobNotOwned)
        );
        assert!(cell
            .scope_mcp_arguments(
                "get_restore_operation",
                serde_json::json!({"graphId": "graph-a", "operationId": owned.job_id}),
                &jobs,
            )
            .is_ok());
        assert_eq!(
            cell.scope_mcp_arguments(
                "get_restore_operation",
                serde_json::json!({
                    "graphId": "graph-a",
                    "operationId": foreign.job_id,
                    "jobId": owned.job_id,
                }),
                &jobs,
            ),
            Err(CellGraphBoundaryError::JobNotOwned)
        );
        assert!(matches!(
            cell.scope_mcp_arguments(
                "get_restore_operation",
                serde_json::json!({
                    "graphId": "graph-a",
                    "operationId": owned.job_id,
                    "operation_id": foreign.job_id,
                }),
                &jobs,
            ),
            Err(CellGraphBoundaryError::InvalidRequest(_))
        ));
        assert_eq!(
            cell.scope_mcp_arguments(
                "get_restore_operation",
                serde_json::json!({"graphId": "graph-b", "operationId": owned.job_id}),
                &jobs,
            ),
            Err(CellGraphBoundaryError::GraphMismatch)
        );
        assert_eq!(
            cell.scope_mcp_arguments(
                "cancel_restore_operation",
                serde_json::json!({"graphId": "graph-a", "operationId": foreign.job_id}),
                &jobs,
            ),
            Err(CellGraphBoundaryError::JobNotOwned)
        );
        assert!(matches!(
            cell.scope_mcp_arguments(
                "get_restore_operation",
                serde_json::json!({"graphId": "graph-a"}),
                &jobs,
            ),
            Err(CellGraphBoundaryError::InvalidRequest(_))
        ));

        let _ = fs::remove_dir_all(jobs_dir);
    }
}

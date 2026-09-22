//! Direct, read-only DynamoDB registry preflight for hosted gardend cells.
//!
//! This runs before durable hydrate. A missing, tombstoned, stale-generation,
//! or otherwise mismatched projection makes startup fail without touching graph
//! storage. Desktop and unbound local headless mode remain unaffected.

#[cfg(all(feature = "headless", not(feature = "desktop")))]
use std::collections::HashMap;
use std::sync::OnceLock;

#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct CellRegistryBinding {
    pub(crate) owner: String,
    pub(crate) graph_id: String,
    pub(crate) generation: u64,
    pub(crate) lifecycle_state: String,
    pub(crate) registry_revision: u64,
}

impl CellRegistryBinding {
    fn validate(self) -> Result<Self, String> {
        let (kind, subject) = self
            .owner
            .split_once(':')
            .ok_or_else(|| "GARDEN_CELL_OWNER must be a typed principal id".to_string())?;
        if !matches!(kind, "user" | "agent" | "service" | "organization") || subject.is_empty() {
            return Err("GARDEN_CELL_OWNER must be a valid typed principal id".to_string());
        }
        crate::ids::validate_local_id(&self.graph_id, "GARDEN_CELL_GRAPH_ID")
            .map_err(|error| format!("invalid GARDEN_CELL_GRAPH_ID: {error}"))?;
        if self.generation == 0 {
            return Err("GARDEN_CELL_GRAPH_GENERATION must be positive".to_string());
        }
        if self.registry_revision == 0 {
            return Err("GARDEN_CELL_REGISTRY_REVISION must be positive".to_string());
        }
        if !matches!(
            self.lifecycle_state.as_str(),
            "provisioning" | "active" | "repairing"
        ) {
            return Err(format!(
                "graph lifecycle '{}' is not cell-readable",
                self.lifecycle_state
            ));
        }
        Ok(self)
    }
}

static PREFLIGHT: OnceLock<Result<Option<CellRegistryBinding>, String>> = OnceLock::new();

/// Run once, synchronously, before hydrate. Subsequent calls return the same
/// observation so setup and background work cannot disagree about authority.
#[cfg(all(feature = "headless", not(feature = "desktop")))]
pub(crate) fn preflight_before_storage() -> Result<Option<CellRegistryBinding>, String> {
    PREFLIGHT
        .get_or_init(run_preflight)
        .as_ref()
        .map(Clone::clone)
        .map_err(Clone::clone)
}

#[cfg(any(not(feature = "headless"), feature = "desktop"))]
pub(crate) fn preflight_before_storage() -> Result<Option<CellRegistryBinding>, String> {
    Ok(None)
}

pub(crate) fn verified_binding() -> Result<Option<CellRegistryBinding>, String> {
    preflight_before_storage()
}

#[cfg(all(feature = "headless", not(feature = "desktop")))]
fn run_preflight() -> Result<Option<CellRegistryBinding>, String> {
    // GARDEN_CELL_GRAPH_ID predates the owner-scoped registry and is still set
    // on legacy `/g/{graph_id}` cells. Only the opaque physical cell identity
    // marks the new contract; once it is present, every owner/generation field
    // below is mandatory and the preflight fails closed on a partial envelope.
    let cell_id = env("GARDEN_CELL_ID");
    if !owner_scoped_cell(cell_id.as_deref()) {
        return Ok(None);
    }
    let graph_id = required_env("GARDEN_CELL_GRAPH_ID")?;
    let expected = CellRegistryBinding {
        owner: required_env("GARDEN_CELL_OWNER")?,
        graph_id,
        generation: parse_positive_env("GARDEN_CELL_GRAPH_GENERATION")?,
        lifecycle_state: "active".to_string(),
        registry_revision: parse_positive_env("GARDEN_CELL_REGISTRY_REVISION")?,
    }
    .validate()?;

    // Deterministic local-process seam used by the real E2E harness. Production
    // never sets it and always performs the strongly consistent DynamoDB read.
    let observed = if let Some(snapshot) = env("GARDEN_CELL_REGISTRY_SNAPSHOT_JSON") {
        serde_json::from_str::<CellRegistryBinding>(&snapshot)
            .map_err(|error| format!("parse GARDEN_CELL_REGISTRY_SNAPSHOT_JSON: {error}"))?
            .validate()?
    } else {
        let table = required_env("GARDEN_CELL_REGISTRY_TABLE")?;
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|error| format!("build registry preflight runtime: {error}"))?;
        runtime.block_on(read_projection(&table, &expected.owner, &expected.graph_id))?
    };
    if observed.owner != expected.owner
        || observed.graph_id != expected.graph_id
        || observed.generation != expected.generation
        || observed.registry_revision != expected.registry_revision
    {
        return Err(format!(
            "cell registry projection mismatch for owner={} graph={} generation={} revision={}",
            expected.owner, expected.graph_id, expected.generation, expected.registry_revision
        ));
    }
    Ok(Some(observed))
}

#[cfg(all(feature = "headless", not(feature = "desktop")))]
async fn read_projection(
    table: &str,
    owner: &str,
    graph_id: &str,
) -> Result<CellRegistryBinding, String> {
    use aws_sdk_dynamodb::types::AttributeValue;
    use aws_smithy_http_client::tls;

    let https = aws_smithy_http_client::Builder::new()
        .tls_provider(tls::Provider::Rustls(
            tls::rustls_provider::CryptoMode::Ring,
        ))
        .build_https();
    let mut loader = aws_config::defaults(aws_config::BehaviorVersion::latest()).http_client(https);
    if let Some(endpoint) = env("GARDEN_DYNAMODB_ENDPOINT") {
        loader = loader.endpoint_url(endpoint);
    }
    let config = loader.load().await;
    let client = aws_sdk_dynamodb::Client::new(&config);
    let output = client
        .get_item()
        .table_name(table)
        .key("pk", AttributeValue::S(format!("GRAPH#{owner}#{graph_id}")))
        .consistent_read(true)
        .send()
        .await
        .map_err(|error| format!("read cell registry projection: {error}"))?;
    let item = output
        .item()
        .ok_or_else(|| "cell registry projection does not exist".to_string())?;
    if let Some(json) = attr_s(item, "projection_json") {
        return serde_json::from_str::<CellRegistryBinding>(&json)
            .map_err(|error| format!("decode projection_json: {error}"))?
            .validate();
    }
    CellRegistryBinding {
        owner: required_attr(item, "owner")?,
        graph_id: required_attr(item, "graph_id")?,
        generation: required_attr(item, "generation")?
            .parse()
            .map_err(|_| "projection generation is not an integer".to_string())?,
        lifecycle_state: required_attr(item, "lifecycle_state")?,
        registry_revision: required_attr(item, "registry_revision")?
            .parse()
            .map_err(|_| "projection registry_revision is not an integer".to_string())?,
    }
    .validate()
}

#[cfg(all(feature = "headless", not(feature = "desktop")))]
fn attr_s(
    item: &HashMap<String, aws_sdk_dynamodb::types::AttributeValue>,
    key: &str,
) -> Option<String> {
    item.get(key).and_then(|value| {
        value
            .as_s()
            .ok()
            .cloned()
            .or_else(|| value.as_n().ok().cloned())
    })
}

#[cfg(all(feature = "headless", not(feature = "desktop")))]
fn required_attr(
    item: &HashMap<String, aws_sdk_dynamodb::types::AttributeValue>,
    key: &str,
) -> Result<String, String> {
    attr_s(item, key).ok_or_else(|| format!("cell registry projection is missing {key}"))
}

fn env(name: &str) -> Option<String> {
    std::env::var(name)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

fn required_env(name: &str) -> Result<String, String> {
    env(name).ok_or_else(|| format!("{name} is required for a bound gardend cell"))
}

fn parse_positive_env(name: &str) -> Result<u64, String> {
    let value = required_env(name)?;
    value
        .parse::<u64>()
        .ok()
        .filter(|value| *value > 0)
        .ok_or_else(|| format!("{name} must be a positive integer"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn projection_validation_rejects_tombstones_and_untyped_owner() {
        let valid = CellRegistryBinding {
            owner: "user:u-1".into(),
            graph_id: "notes".into(),
            generation: 1,
            lifecycle_state: "active".into(),
            registry_revision: 2,
        };
        assert!(valid.clone().validate().is_ok());
        let mut tombstone = valid.clone();
        tombstone.lifecycle_state = "tombstoned".into();
        assert!(tombstone.validate().is_err());
        let mut untyped = valid;
        untyped.owner = "u-1".into();
        assert!(untyped.validate().is_err());
    }

    #[test]
    fn legacy_graph_id_alone_is_not_an_owner_scoped_registry_binding() {
        // The process-level decision is intentionally keyed by GARDEN_CELL_ID,
        // not GARDEN_CELL_GRAPH_ID. Real-process U8 tests cover the legacy path;
        // the owner-scoped parity harness supplies both values.
        assert!(owner_scoped_cell(Some("c-opaque")));
        assert!(!owner_scoped_cell(Some("  ")));
        assert!(!owner_scoped_cell(None));
    }
}

fn owner_scoped_cell(cell_id: Option<&str>) -> bool {
    cell_id.is_some_and(|value| !value.trim().is_empty())
}

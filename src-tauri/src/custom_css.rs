//! Explicit graph-shared stylesheet source. Application/consent is a host concern.
//! The workspace Y.Doc is the sole authority; no profile CSS store or new root.
use crate::{
    app_error::{AppError, AppResult},
    app_runtime::AppHandle,
    crdt_engine::{persistence_coordinator::GraphPersistenceCoordinator, workspace_ops},
    crdt_queue::EnqueueCrdtOperationInput,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
#[cfg(feature = "desktop")]
use tauri::Manager;
use yrs::{Any, Map, Out, ReadTxn, Transact, TransactionMut, WriteTxn};

pub(crate) const MAX_CSS_BYTES: usize = 65_536;
const MAX_REVISION: u64 = 9_007_199_254_740_991;
const STATE_MAP: &str = "customCss";
const RECEIPTS_MAP: &str = "customCssReceipts";

#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct CssMutation {
    pub graph_incarnation: String,
    pub expected_content_sha256: String,
    pub expected_revision: u64,
    pub css_text: String,
    #[serde(rename = "cssOperationId", alias = "operationId")]
    pub operation_id: String,
}
#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct CssState {
    schema_version: u32,
    graph_incarnation: String,
    css_text: String,
    revision: u64,
}
fn conflict(message: impl Into<String>) -> AppError {
    AppError::conflict(message).with_code("custom_css_conflict")
}
fn validate(input: &CssMutation) -> Result<(), String> {
    crate::ids::validate_local_id(&input.operation_id, "operationId")?;
    if input.graph_incarnation.is_empty() || input.expected_revision > MAX_REVISION {
        return Err("custom CSS requires a graph incarnation and safe integer revision".into());
    }
    if input.css_text.len() > MAX_CSS_BYTES {
        return Err("custom CSS exceeds 65536 UTF-8 bytes".into());
    }
    if input.expected_content_sha256.len() != 64
        || !input
            .expected_content_sha256
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    {
        return Err("expectedContentSha256 must be a lowercase SHA-256".into());
    }
    Ok(())
}
fn read_state(txn: &impl ReadTxn, incarnation: &str) -> Result<CssState, String> {
    let stored = txn.get_map(STATE_MAP).and_then(|map| map.get(txn, "state"));
    let state = match stored {
        None => CssState {
            schema_version: 1,
            graph_incarnation: incarnation.into(),
            css_text: String::new(),
            revision: 0,
        },
        Some(Out::Any(Any::String(text))) => serde_json::from_str::<CssState>(&text)
            .map_err(|e| format!("malformed custom CSS authority: {e}"))?,
        Some(_) => return Err("malformed custom CSS authority: expected exact JSON string".into()),
    };
    if state.schema_version != 1
        || state.graph_incarnation != incarnation
        || state.revision > MAX_REVISION
        || state.css_text.len() > MAX_CSS_BYTES
    {
        return Err(
            "unsupported, oversized or stale custom CSS authority; raw state was not rewritten"
                .into(),
        );
    }
    Ok(state)
}
fn snapshot(graph: &str, state: &CssState) -> Value {
    json!({"schemaVersion":1,"scope":"graph","graphId":graph,"graphIncarnation":state.graph_incarnation,"cssText":state.css_text,"contentHashSha256":crate::artifact_text_service::hash(state.css_text.as_bytes()),"revision":state.revision})
}
pub(crate) fn update(
    txn: &mut TransactionMut<'_>,
    graph: &str,
    payload: &Value,
) -> Result<Value, String> {
    let mut value = payload.clone();
    if let Some(object) = value.as_object_mut() {
        object.remove(crate::crdt_queue::RECOVERED_OPERATION_PAYLOAD_KEY);
    }
    let input: CssMutation = serde_json::from_value(value).map_err(|e| e.to_string())?;
    validate(&input)?;
    let current = read_state(&*txn, &input.graph_incarnation)?;
    let digest =
        crate::artifact_text_service::hash(&serde_json::to_vec(&input).map_err(|e| e.to_string())?);
    let prior = txn
        .get_map(RECEIPTS_MAP)
        .and_then(|map| map.get(&*txn, &input.operation_id));
    if let Some(prior) = prior {
        if !matches!(prior, Out::Any(Any::String(ref old)) if old.as_ref() == digest) {
            return Err(
                "custom_css_conflict: operation ID has a different retained request".into(),
            );
        }
        let mut result = snapshot(graph, &current);
        result["replayedOperationId"] = json!(input.operation_id);
        return Ok(result);
    }
    if current.revision != input.expected_revision
        || crate::artifact_text_service::hash(current.css_text.as_bytes())
            != input.expected_content_sha256
    {
        return Err(
            "custom_css_conflict: stylesheet changed; reread before a new operation".into(),
        );
    }
    let next = CssState {
        schema_version: 1,
        graph_incarnation: input.graph_incarnation,
        css_text: input.css_text,
        revision: current
            .revision
            .checked_add(1)
            .filter(|v| *v <= MAX_REVISION)
            .ok_or("custom CSS revision exhausted")?,
    };
    let encoded = serde_json::to_string(&next).map_err(|e| e.to_string())?;
    // All fallible preflight precedes mutation. State and exact-request receipt
    // share one persisted/broadcast transaction; replay never reapplies old CSS.
    txn.get_or_insert_map(STATE_MAP)
        .insert(txn, "state", encoded);
    txn.get_or_insert_map(RECEIPTS_MAP)
        .insert(txn, input.operation_id.as_str(), digest);
    Ok(snapshot(graph, &next))
}
pub(crate) async fn read(app: &AppHandle, graph: &str) -> AppResult<Value> {
    let coordinator = app.state::<GraphPersistenceCoordinator>();
    let lease = coordinator
        .acquire_hot_write(graph)
        .await
        .map_err(AppError::storage)?;
    lease.declare_rdf_read_only();
    let (dir, record) = crate::graph_record_store::read_graph_record_no_heal(app, graph)?;
    let incarnation = record
        .incarnation_id
        .ok_or_else(|| conflict("graph incarnation is missing"))?;
    let room = workspace_ops::workspace_room(app, graph, &dir)
        .await
        .map_err(AppError::storage)?;
    room.with_doc(|doc| {
        read_state(&doc.transact(), &incarnation).map(|state| snapshot(graph, &state))
    })
    .await
    .map_err(conflict)
}
pub(crate) async fn submit(app: AppHandle, graph: String, input: CssMutation) -> AppResult<Value> {
    validate(&input).map_err(AppError::validation)?;
    crate::crdt_queue::enqueue_crdt_operation(
        app,
        EnqueueCrdtOperationInput {
            kind: "workspace.setCustomCss".into(),
            graph_id: graph,
            document_id: None,
            payload: serde_json::to_value(input)
                .map_err(|e| AppError::serialization(e.to_string()))?,
        },
    )
    .await
    .map_err(|error| {
        if error.contains("custom_css_conflict") || error.contains("stale graph incarnation") {
            conflict(error)
        } else if error.contains("timed out") {
            AppError::deadline(error).with_code("custom_css_tail_pending")
        } else {
            AppError::storage(error)
        }
    })
}
pub(crate) fn guide() -> Value {
    json!({"schemaVersion":1,"scope":"graph-shared-source; local explicit application consent","maxCssUtf8Bytes":MAX_CSS_BYTES,"optionalDiscovery":{"method":"tools/list","params":{"optionalCapabilities":["custom-css"]}},"operations":{"read":"read_custom_css","write":"write_custom_css","reset":"write_custom_css with cssText empty"},"concurrency":"Use the exact observed graphIncarnation, revision and contentHashSha256. Retain operationId and every field after an uncertain response; an exact replay returns current state, which may be newer. Changed duplicate IDs are refused.","styling":{"target":"document light DOM and inherited CSS custom properties; selectors do not cross Lit shadow roots","tokens":["--mn-font-chrome","--mn-font-sans","--mn-font-mono","--mn-radius-control","--mn-radius-surface"],"parts":{"mn-top-bar":["masthead","app-switcher","breadcrumbs","actions"]},"examples":[":root { --mn-font-chrome: system-ui; --mn-radius-control: 10px; }","mn-top-bar::part(masthead) { letter-spacing: .05em; }"],"limitations":["Headless Garden does not inspect computed DOM, screenshots or layout.","CSS is stored exactly; browser parsers may ignore invalid declarations. No all-or-nothing syntax validation.","Application is local opt-in for an exact content hash; imports and other writers must not auto-apply.","CSS url() and @import are not fetched by this service; actual browser/desktop CSP governs any host requests.","Presentation does not grant capabilities or expose backend-only data."]}})
}
pub(crate) async fn mcp_read(app: AppHandle, args: &Value) -> AppResult<Value> {
    let graph = crate::mcp_utils::mcp_required_graph_id(args).map_err(AppError::validation)?;
    let mut value = read(&app, &graph).await?;
    value["guide"] = guide();
    Ok(value)
}
pub(crate) async fn mcp_capability(app: AppHandle, args: &Value) -> AppResult<Value> {
    match args.get("action").and_then(Value::as_str).unwrap_or("guide") {
        "guide" => Ok(guide()),
        "read" => mcp_read(app, args).await,
        "replace" | "reset" => {
            let reset = args["action"] == "reset";
            let mut value = args.clone();
            if let Some(object) = value.as_object_mut() {
                object.remove("action");
                if reset {
                    if object.contains_key("cssText") { return Err(AppError::validation("reset must not include cssText")); }
                    object.insert("cssText".into(), json!(""));
                }
            }
            mcp_write(app, &value).await
        }
        _ => Err(AppError::validation("action must be guide, read, replace or reset")),
    }
}
pub(crate) async fn mcp_write(app: AppHandle, args: &Value) -> AppResult<Value> {
    let graph = crate::mcp_utils::mcp_required_graph_id(args).map_err(AppError::validation)?;
    let mut value = args.clone();
    if let Some(object) = value.as_object_mut() {
        object.remove("graph_id");
        object.remove("graphId");
    }
    let input = serde_json::from_value(value)
        .map_err(|e| AppError::validation(format!("invalid CSS request: {e}")))?;
    submit(app, graph, input).await
}

#[cfg(test)]
#[path = "custom_css_tests.rs"]
mod tests;

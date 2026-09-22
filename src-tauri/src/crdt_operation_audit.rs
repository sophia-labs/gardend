use crate::app_runtime::AppHandle;
use crate::{
    crdt_operation_types::{CrdtOperation, EnqueuedCrdtOutcome},
    loopback_audit_log::{append_loopback_audit_event, LoopbackAuditEvent},
};

pub(crate) fn audit_crdt_operation(
    app: &AppHandle,
    operation: &CrdtOperation,
    outcome: &Result<EnqueuedCrdtOutcome, String>,
) {
    let (outcome_label, error) = match outcome {
        Ok(_) => ("succeeded", None),
        Err(error) => ("failed", Some(error.as_str())),
    };
    let event = crdt_operation_audit_event(operation, outcome_label, error);
    if let Err(error) = append_loopback_audit_event(app, &event) {
        log::debug!(
            "failed to append CRDT audit event for {}: {error}",
            operation.operation_id
        );
    }
}

fn crdt_operation_audit_event(
    operation: &CrdtOperation,
    outcome: &str,
    error: Option<&str>,
) -> LoopbackAuditEvent {
    let mut details = serde_json::json!({
        "operationId": &operation.operation_id,
        "kind": &operation.kind,
        "graphId": &operation.graph_id,
        "documentId": &operation.document_id,
        "payload": payload_audit_shape(&operation.payload),
    });
    if let Some(error) = error {
        details["error"] = serde_json::Value::String(error.to_string());
    }
    LoopbackAuditEvent::new(
        "crdt.operation",
        &operation.kind,
        "local-runtime",
        outcome,
        Some(operation.operation_id.clone()),
        details,
    )
}

fn payload_audit_shape(payload: &serde_json::Value) -> serde_json::Value {
    match payload {
        serde_json::Value::Object(object) => {
            let mut keys = object.keys().cloned().collect::<Vec<_>>();
            keys.sort();
            serde_json::json!({
                "type": "object",
                "keys": keys,
                "keyCount": object.len(),
                "encodedBytes": payload.to_string().len(),
            })
        }
        serde_json::Value::Array(array) => serde_json::json!({
            "type": "array",
            "itemCount": array.len(),
            "encodedBytes": payload.to_string().len(),
        }),
        serde_json::Value::Null => serde_json::json!({
            "type": "null",
            "encodedBytes": 4,
        }),
        serde_json::Value::String(value) => serde_json::json!({
            "type": "string",
            "encodedBytes": value.len(),
        }),
        serde_json::Value::Number(_) => serde_json::json!({
            "type": "number",
            "encodedBytes": payload.to_string().len(),
        }),
        serde_json::Value::Bool(_) => serde_json::json!({
            "type": "boolean",
            "encodedBytes": payload.to_string().len(),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crdt_operation_audit_event_redacts_payload_values() {
        let operation = CrdtOperation {
            operation_id: "op-1".to_string(),
            kind: "document.write".to_string(),
            graph_id: "graph-1".to_string(),
            document_id: Some("doc-1".to_string()),
            payload: serde_json::json!({
                "title": "Secret title",
                "dataBase64": "c2VjcmV0LWJ5dGVz",
                "blocks": ["one", "two"],
            }),
            enqueue_timestamp: "0".to_string(),
        };

        let event = crdt_operation_audit_event(&operation, "succeeded", None);
        let event_json = serde_json::to_value(&event).expect("event json");

        assert_eq!(event_json["category"], "crdt.operation");
        assert_eq!(event_json["action"], "document.write");
        assert_eq!(event_json["targetId"], "op-1");
        assert_eq!(event_json["details"]["payload"]["keyCount"], 3);
        assert_eq!(
            event_json["details"]["payload"]["keys"],
            serde_json::json!(["blocks", "dataBase64", "title"])
        );
        let line = event.to_json_line();
        assert!(!line.contains("Secret title"));
        assert!(!line.contains("c2VjcmV0LWJ5dGVz"));
        assert!(!line.contains("one"));
    }

    #[test]
    fn crdt_operation_audit_event_records_failure_without_payload_values() {
        let operation = CrdtOperation {
            operation_id: "op-2".to_string(),
            kind: "block.delete".to_string(),
            graph_id: "graph-1".to_string(),
            document_id: Some("doc-1".to_string()),
            payload: serde_json::json!(["block-1", "block-2"]),
            enqueue_timestamp: "0".to_string(),
        };

        let event = crdt_operation_audit_event(&operation, "failed", Some("boom"));
        let event_json = serde_json::to_value(&event).expect("event json");

        assert_eq!(event_json["outcome"], "failed");
        assert_eq!(event_json["details"]["error"], "boom");
        assert_eq!(event_json["details"]["payload"]["itemCount"], 2);
        assert!(!event.to_json_line().contains("block-1"));
    }
}

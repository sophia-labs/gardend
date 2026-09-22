use crate::app_runtime::AppHandle;
use crate::{
    clock::timestamp,
    paths::loopback_audit_log_path,
    storage::{append_secret_line, display_path},
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    fs,
    io::{BufRead, BufReader},
};
use uuid::Uuid;

const DEFAULT_AUDIT_LIMIT: usize = 100;
const MAX_AUDIT_LIMIT: usize = 1000;

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ListLoopbackAuditEventsInput {
    limit: Option<usize>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct LoopbackAuditEvent {
    event_id: String,
    occurred_at: String,
    category: String,
    action: String,
    actor: String,
    outcome: String,
    target_id: Option<String>,
    details: Value,
}

impl LoopbackAuditEvent {
    pub(crate) fn new(
        category: &str,
        action: &str,
        actor: &str,
        outcome: &str,
        target_id: Option<String>,
        details: Value,
    ) -> Self {
        Self {
            event_id: Uuid::new_v4().to_string(),
            occurred_at: timestamp(),
            category: category.to_string(),
            action: action.to_string(),
            actor: actor.to_string(),
            outcome: outcome.to_string(),
            target_id,
            details,
        }
    }

    pub(crate) fn to_json_line(&self) -> String {
        serde_json::to_string(self).unwrap_or_default()
    }
}

#[cfg_attr(feature = "desktop", tauri::command)]
pub(crate) fn list_loopback_audit_events(
    app: AppHandle,
    input: Option<ListLoopbackAuditEventsInput>,
) -> Result<Vec<LoopbackAuditEvent>, String> {
    let limit = input
        .and_then(|input| input.limit)
        .unwrap_or(DEFAULT_AUDIT_LIMIT)
        .clamp(1, MAX_AUDIT_LIMIT);
    read_loopback_audit_events(&app, limit)
}

pub(crate) fn append_loopback_audit_event(
    app: &AppHandle,
    event: &LoopbackAuditEvent,
) -> Result<(), String> {
    let path = loopback_audit_log_path(app)?;
    append_loopback_audit_event_to_path(&path, event)
}

fn read_loopback_audit_events(
    app: &AppHandle,
    limit: usize,
) -> Result<Vec<LoopbackAuditEvent>, String> {
    let path = loopback_audit_log_path(app)?;
    if !path.is_file() {
        return Ok(Vec::new());
    }

    let file =
        fs::File::open(&path).map_err(|error| format!("open {}: {error}", display_path(&path)))?;
    let reader = BufReader::new(file);
    let mut events = Vec::new();
    for line in reader.lines() {
        let line = line.map_err(|error| format!("read {}: {error}", display_path(&path)))?;
        if line.trim().is_empty() {
            continue;
        }
        let event = serde_json::from_str::<LoopbackAuditEvent>(&line)
            .map_err(|error| format!("parse {}: {error}", display_path(&path)))?;
        events.push(event);
    }
    let skip = events.len().saturating_sub(limit);
    Ok(events.into_iter().skip(skip).collect())
}

fn append_loopback_audit_event_to_path(
    path: &std::path::Path,
    event: &LoopbackAuditEvent,
) -> Result<(), String> {
    let line = event.to_json_line();
    if line.is_empty() {
        return Err("serialize audit event failed".to_string());
    }
    append_secret_line(path, &line).map_err(Into::into)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_audit_path(name: &str) -> std::path::PathBuf {
        let suffix = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        std::env::temp_dir().join(format!("mnemosyne-audit-{name}-{suffix}.jsonl"))
    }

    #[test]
    fn audit_event_builder_preserves_redacted_details() {
        let event = LoopbackAuditEvent::new(
            "loopback.token",
            "create",
            "tauri",
            "succeeded",
            Some("token-1".to_string()),
            json!({
                "grantProfileId": "read-only",
                "scopeCount": 3,
            }),
        );

        assert_eq!(event.category, "loopback.token");
        assert_eq!(event.action, "create");
        assert_eq!(event.target_id.as_deref(), Some("token-1"));
        assert!(event.details.get("grantProfileId").is_some());
        assert!(event.details.get("token").is_none());
        assert!(event.details.get("tokenHash").is_none());
    }

    #[test]
    fn append_audit_event_writes_jsonl_secret_file() {
        let path = temp_audit_path("append");
        let event = LoopbackAuditEvent::new(
            "loopback.token",
            "revoke",
            "tauri",
            "succeeded",
            Some("token-1".to_string()),
            json!({ "scopeCount": 2 }),
        );

        append_loopback_audit_event_to_path(&path, &event).expect("append audit event");

        let raw = fs::read_to_string(&path).expect("read audit log");
        assert!(raw.ends_with('\n'));
        let parsed = serde_json::from_str::<LoopbackAuditEvent>(raw.trim()).expect("parse event");
        assert_eq!(parsed.action, "revoke");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(&path).expect("metadata").permissions().mode() & 0o777;
            assert_eq!(mode, 0o600);
        }
        let _ = fs::remove_file(path);
    }

    #[test]
    fn append_audit_event_reports_write_failure() {
        let path = temp_audit_path("append-failure");
        fs::create_dir_all(&path).expect("create directory at audit path");
        let event = LoopbackAuditEvent::new(
            "loopback.token",
            "revoke",
            "tauri",
            "succeeded",
            Some("token-1".to_string()),
            json!({ "scopeCount": 2 }),
        );

        let error =
            append_loopback_audit_event_to_path(&path, &event).expect_err("append should fail");

        assert!(error.contains("open "));
        let _ = fs::remove_dir_all(path);
    }
}

use crate::app_runtime::AppHandle;
use crate::{
    profile_paths,
    storage::{read_json, remove_file_if_exists, write_json},
};
use serde::Serialize;
use serde_json::Value;
use std::path::PathBuf;

const CHAT_DIR: &str = "chat";

// Local chat persistence: one JSON document per session under
// `profile_dir/chat/<session_id>.json`. The Pi agent runs in the webview; this
// is purely durable storage so sessions survive an app restart. The session
// payload — including the Pi `AgentMessage[]` transcript — is stored opaquely
// as `serde_json::Value`: that shape is owned and versioned by the frontend, so
// Rust only reaches into well-known top-level keys (`session_id`, `title`,
// timestamps, `messages`) to build the sidebar listing. Not stored 0o600 —
// chat content is not a secret, so `write_json` (the non-secret tier) is right.

/// Reject ids that could escape the chat dir or collide with the filesystem.
/// Session ids are client-generated UUIDs; anything outside this charset is a
/// bug or an attack, so we refuse rather than sanitize.
fn is_safe_session_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 128
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

fn chat_dir(app: &AppHandle) -> Result<PathBuf, String> {
    Ok(profile_paths::profile_dir(app)?.join(CHAT_DIR))
}

fn session_path(app: &AppHandle, id: &str) -> Result<PathBuf, String> {
    if !is_safe_session_id(id) {
        return Err(format!("invalid session id: {id}"));
    }
    Ok(chat_dir(app)?.join(format!("{id}.json")))
}

/// Non-transcript summary for the session sidebar. Field names mirror the
/// frontend's session shape so the adapter mapping stays near-identity.
#[derive(Debug, Serialize, PartialEq)]
pub(crate) struct ChatSessionMeta {
    session_id: String,
    title: Option<String>,
    created_at: Option<String>,
    updated_at: Option<String>,
    graph_id: Option<String>,
    message_count: usize,
}

/// Best-effort projection of a stored session document to its sidebar meta.
/// Returns None only when `session_id` is absent/non-string (a malformed file).
fn project_meta(value: &Value) -> Option<ChatSessionMeta> {
    let session_id = value.get("session_id")?.as_str()?.to_string();
    let str_field = |key: &str| value.get(key).and_then(Value::as_str).map(String::from);
    let message_count = value
        .get("messages")
        .and_then(Value::as_array)
        .map(Vec::len)
        .unwrap_or(0);
    Some(ChatSessionMeta {
        session_id,
        title: str_field("title"),
        created_at: str_field("created_at"),
        updated_at: str_field("updated_at"),
        graph_id: str_field("graph_id"),
        message_count,
    })
}

#[cfg_attr(feature = "desktop", tauri::command)]
pub(crate) fn chat_persist_session(app: AppHandle, session: Value) -> Result<(), String> {
    let id = session
        .get("session_id")
        .and_then(Value::as_str)
        .ok_or("session.session_id missing or not a string")?
        .to_string();
    let path = session_path(&app, &id)?;
    write_json(&path, &session).map_err(Into::into)
}

#[cfg_attr(feature = "desktop", tauri::command)]
pub(crate) fn chat_load_session(
    app: AppHandle,
    session_id: String,
) -> Result<Option<Value>, String> {
    let path = session_path(&app, &session_id)?;
    if !path.exists() {
        return Ok(None);
    }
    let value: Value = read_json(&path)?;
    Ok(Some(value))
}

#[cfg_attr(feature = "desktop", tauri::command)]
pub(crate) fn chat_delete_session(app: AppHandle, session_id: String) -> Result<(), String> {
    let path = session_path(&app, &session_id)?;
    remove_file_if_exists(&path).map(|_| ()).map_err(Into::into)
}

#[cfg_attr(feature = "desktop", tauri::command)]
pub(crate) fn chat_load_sessions(app: AppHandle) -> Result<Vec<ChatSessionMeta>, String> {
    let dir = chat_dir(&app)?;
    if !dir.exists() {
        return Ok(Vec::new());
    }
    let entries = std::fs::read_dir(&dir).map_err(|error| format!("read chat dir: {error}"))?;
    let mut metas: Vec<ChatSessionMeta> = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|ext| ext.to_str()) != Some("json") {
            continue;
        }
        // Skip — don't fail the whole listing on one corrupt/foreign file.
        match read_json::<Value>(&path) {
            Ok(value) => match project_meta(&value) {
                Some(meta) => metas.push(meta),
                None => log::warn!(
                    "chat_load_sessions: skipping malformed session file {}",
                    path.display()
                ),
            },
            Err(error) => log::warn!(
                "chat_load_sessions: skipping unparseable session file {}: {error}",
                path.display()
            ),
        }
    }
    // Newest first. `updated_at` is an ISO-8601 string, which sorts lexically.
    metas.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));
    Ok(metas)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::{
        fs,
        path::PathBuf,
        time::{SystemTime, UNIX_EPOCH},
    };

    fn temp_path(name: &str) -> PathBuf {
        let suffix = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        std::env::temp_dir().join(format!("chat-persist-{name}-{suffix}.json"))
    }

    fn sample(id: &str, updated: &str) -> Value {
        json!({
            "session_id": id,
            "title": "A chat",
            "created_at": "2026-06-03T10:00:00.000Z",
            "updated_at": updated,
            "graph_id": "default",
            "messages": [{"role": "user"}, {"role": "assistant"}],
        })
    }

    #[test]
    fn rejects_unsafe_session_ids() {
        assert!(is_safe_session_id("0b3f-uuid_v4-ABC"));
        assert!(!is_safe_session_id(""));
        assert!(!is_safe_session_id("../escape"));
        assert!(!is_safe_session_id("a/b"));
        assert!(!is_safe_session_id("dot.dot"));
        assert!(!is_safe_session_id(&"x".repeat(129)));
    }

    #[test]
    fn project_meta_reads_known_keys() {
        let meta = project_meta(&sample("s1", "2026-06-03T11:00:00.000Z")).unwrap();
        assert_eq!(meta.session_id, "s1");
        assert_eq!(meta.title.as_deref(), Some("A chat"));
        assert_eq!(meta.graph_id.as_deref(), Some("default"));
        assert_eq!(meta.message_count, 2);
    }

    #[test]
    fn project_meta_rejects_missing_id() {
        assert!(project_meta(&json!({"title": "no id"})).is_none());
    }

    #[test]
    fn document_round_trips_opaquely() {
        let path = temp_path("round-trip");
        let doc = sample("s2", "2026-06-03T12:00:00.000Z");
        write_json(&path, &doc).unwrap();
        let read: Value = read_json(&path).unwrap();
        assert_eq!(read, doc);
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn meta_sorts_newest_first() {
        let mut metas = vec![
            project_meta(&sample("old", "2026-06-01T00:00:00.000Z")).unwrap(),
            project_meta(&sample("new", "2026-06-03T00:00:00.000Z")).unwrap(),
            project_meta(&sample("mid", "2026-06-02T00:00:00.000Z")).unwrap(),
        ];
        metas.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));
        let order: Vec<&str> = metas.iter().map(|m| m.session_id.as_str()).collect();
        assert_eq!(order, vec!["new", "mid", "old"]);
    }
}

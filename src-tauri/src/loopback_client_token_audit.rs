use crate::{
    loopback_audit_log::LoopbackAuditEvent, loopback_client_token_types::LoopbackClientTokenRecord,
};

pub(super) fn token_audit_event(
    action: &str,
    outcome: &str,
    record: &LoopbackClientTokenRecord,
) -> LoopbackAuditEvent {
    LoopbackAuditEvent::new(
        "loopback.token",
        action,
        "tauri",
        outcome,
        Some(record.token_id.clone()),
        serde_json::json!({
            "label": &record.label,
            "grantProfileId": &record.grant_profile_id,
            "scopeCount": record.scopes.len(),
            "scopes": &record.scopes,
            "createdAt": &record.created_at,
            "expiresAt": &record.expires_at,
            "revokedAt": &record.revoked_at,
        }),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::loopback_client_token_records::hash_loopback_token;

    #[test]
    fn token_audit_event_omits_secret_material() {
        let record = LoopbackClientTokenRecord {
            token_id: "token-1".to_string(),
            label: "Client".to_string(),
            token_hash: hash_loopback_token("sophia-lb-secret"),
            grant_profile_id: Some("read-only".to_string()),
            scopes: vec!["graphs.read".to_string()],
            created_at: "1000".to_string(),
            expires_at: "2000".to_string(),
            revoked_at: None,
        };

        let event = token_audit_event("create", "succeeded", &record);
        let event_json = serde_json::to_value(&event).expect("event json");

        assert_eq!(event_json["category"], "loopback.token");
        assert_eq!(event_json["action"], "create");
        assert_eq!(event_json["targetId"], "token-1");
        assert_eq!(event_json["details"]["scopeCount"], 1);
        assert!(event_json["details"].get("token").is_none());
        assert!(event_json["details"].get("tokenHash").is_none());
        assert!(!event.to_json_line().contains("sophia-lb-secret"));
    }
}

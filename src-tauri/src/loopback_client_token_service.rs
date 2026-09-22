use crate::app_runtime::AppHandle;
use crate::{
    clock::{epoch_millis, timestamp},
    loopback_audit_log::append_loopback_audit_event,
    loopback_client_token_audit::token_audit_event,
    loopback_client_token_records::{
        expires_at_from_days, hash_loopback_token, token_record_matches, token_summary,
    },
    loopback_client_token_scope_resolution::resolve_requested_scopes,
    loopback_client_token_store::{read_token_store, write_token_store},
    loopback_client_token_types::{
        CreateLoopbackClientTokenInput, CreateLoopbackClientTokenResponse,
        LoopbackClientTokenRecord, LoopbackClientTokenSummary, RevokeLoopbackClientTokenInput,
    },
};
use uuid::Uuid;

pub(crate) fn list_client_tokens(
    app: &AppHandle,
) -> Result<Vec<LoopbackClientTokenSummary>, String> {
    let now = epoch_millis();
    Ok(read_token_store(app)?
        .tokens
        .iter()
        .map(|record| token_summary(record, now))
        .collect())
}

pub(crate) fn create_client_token(
    app: &AppHandle,
    input: CreateLoopbackClientTokenInput,
) -> Result<CreateLoopbackClientTokenResponse, String> {
    let CreateLoopbackClientTokenInput {
        label,
        grant_profile_id,
        scopes,
        expires_in_days,
    } = input;
    let label = label
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| "Loopback client".to_string());
    let (grant_profile_id, scopes) = resolve_requested_scopes(grant_profile_id, scopes)?;
    let now = epoch_millis();
    let expires_at = expires_at_from_days(now, expires_in_days)?;
    let token = format!("sophia-lb-{}", Uuid::new_v4().simple());
    let record = LoopbackClientTokenRecord {
        token_id: Uuid::new_v4().to_string(),
        label,
        token_hash: hash_loopback_token(&token),
        grant_profile_id,
        scopes,
        created_at: now.to_string(),
        expires_at,
        revoked_at: None,
    };

    let mut store = read_token_store(app)?;
    store.tokens.push(record.clone());
    write_token_store(app, &store)?;
    append_token_audit_best_effort(app, "create", &record);

    Ok(CreateLoopbackClientTokenResponse {
        token,
        record: token_summary(&record, now),
    })
}

pub(crate) fn revoke_client_token(
    app: &AppHandle,
    input: RevokeLoopbackClientTokenInput,
) -> Result<LoopbackClientTokenSummary, String> {
    let mut store = read_token_store(app)?;
    let token_id = input.token_id.trim();
    let Some(record) = store
        .tokens
        .iter_mut()
        .find(|record| record.token_id == token_id)
    else {
        return Err(format!("loopback client token not found: {token_id}"));
    };
    let outcome = if record.revoked_at.is_none() {
        "succeeded"
    } else {
        "unchanged"
    };
    if record.revoked_at.is_none() {
        record.revoked_at = Some(timestamp());
    }
    let summary = token_summary(record, epoch_millis());
    let audit_event = token_audit_event("revoke", outcome, record);
    write_token_store(app, &store)?;
    if let Err(error) = append_loopback_audit_event(app, &audit_event) {
        log::debug!(
            "failed to append loopback token revoke audit event for {}: {error}",
            summary.token_id
        );
    }
    Ok(summary)
}

fn append_token_audit_best_effort(
    app: &AppHandle,
    action: &str,
    record: &LoopbackClientTokenRecord,
) {
    if let Err(error) =
        append_loopback_audit_event(app, &token_audit_event(action, "succeeded", record))
    {
        log::debug!(
            "failed to append loopback token {action} audit event for {}: {error}",
            record.token_id
        );
    }
}

pub(crate) fn resolve_loopback_client_token(
    app: &AppHandle,
    token: &str,
) -> Result<Option<Vec<String>>, String> {
    let store = read_token_store(app)?;
    let token_hash = hash_loopback_token(token);
    let now = epoch_millis();
    Ok(store
        .tokens
        .iter()
        .find(|record| token_record_matches(record, &token_hash, now))
        .map(|record| record.scopes.clone()))
}

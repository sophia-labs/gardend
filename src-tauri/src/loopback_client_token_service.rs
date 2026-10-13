use crate::app_runtime::AppHandle;
use crate::{
    clock::{epoch_millis, timestamp},
    loopback_audit_log::append_loopback_audit_event_to_path,
    loopback_client_token_audit::token_audit_event,
    loopback_client_token_records::{
        expires_at_from_days, hash_loopback_token, token_record_matches, token_summary,
    },
    loopback_client_token_scope_resolution::resolve_requested_scopes,
    loopback_client_token_store::{read_token_store_in, write_token_store_in},
    loopback_client_token_types::{
        CreateLoopbackClientTokenInput, CreateLoopbackClientTokenResponse,
        LoopbackClientTokenRecord, LoopbackClientTokenSummary, RevokeLoopbackClientTokenInput,
    },
    paths::profile_dir,
    profile_paths::LOOPBACK_AUDIT_LOG_FILE,
};
use std::path::Path;
use uuid::Uuid;

/// Who minted or revoked a token, as the audit log records it.
const TAURI_ACTOR: &str = "tauri";

// The store is a file in the profile directory and every request re-reads it
// (`resolve_loopback_client_token`), so a token created or revoked by any
// writer (the desktop app, or `gardend token ...` against the profile of a
// running gardend) takes effect on the next request with no restart.

pub(crate) fn list_client_tokens(
    app: &AppHandle,
) -> Result<Vec<LoopbackClientTokenSummary>, String> {
    list_client_tokens_in(&profile_dir(app)?)
}

pub(crate) fn list_client_tokens_in(
    profile_dir: &Path,
) -> Result<Vec<LoopbackClientTokenSummary>, String> {
    let now = epoch_millis();
    Ok(read_token_store_in(profile_dir)?
        .tokens
        .iter()
        .map(|record| token_summary(record, now))
        .collect())
}

pub(crate) fn create_client_token(
    app: &AppHandle,
    input: CreateLoopbackClientTokenInput,
) -> Result<CreateLoopbackClientTokenResponse, String> {
    create_client_token_in(&profile_dir(app)?, input, TAURI_ACTOR)
}

pub(crate) fn create_client_token_in(
    profile_dir: &Path,
    input: CreateLoopbackClientTokenInput,
    actor: &str,
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

    let mut store = read_token_store_in(profile_dir)?;
    store.tokens.push(record.clone());
    write_token_store_in(profile_dir, &store)?;
    append_token_audit_best_effort(profile_dir, actor, "create", "succeeded", &record);

    Ok(CreateLoopbackClientTokenResponse {
        token,
        record: token_summary(&record, now),
    })
}

pub(crate) fn revoke_client_token(
    app: &AppHandle,
    input: RevokeLoopbackClientTokenInput,
) -> Result<LoopbackClientTokenSummary, String> {
    revoke_client_token_in(&profile_dir(app)?, input, TAURI_ACTOR)
}

pub(crate) fn revoke_client_token_in(
    profile_dir: &Path,
    input: RevokeLoopbackClientTokenInput,
    actor: &str,
) -> Result<LoopbackClientTokenSummary, String> {
    let mut store = read_token_store_in(profile_dir)?;
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
    let record = record.clone();
    write_token_store_in(profile_dir, &store)?;
    append_token_audit_best_effort(profile_dir, actor, "revoke", outcome, &record);
    Ok(token_summary(&record, epoch_millis()))
}

fn append_token_audit_best_effort(
    profile_dir: &Path,
    actor: &str,
    action: &str,
    outcome: &str,
    record: &LoopbackClientTokenRecord,
) {
    let event = token_audit_event(actor, action, outcome, record);
    if let Err(error) =
        append_loopback_audit_event_to_path(&profile_dir.join(LOOPBACK_AUDIT_LOG_FILE), &event)
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
    resolve_loopback_client_token_in(&profile_dir(app)?, token)
}

pub(crate) fn resolve_loopback_client_token_in(
    profile_dir: &Path,
    token: &str,
) -> Result<Option<Vec<String>>, String> {
    let store = read_token_store_in(profile_dir)?;
    let token_hash = hash_loopback_token(token);
    let now = epoch_millis();
    Ok(store
        .tokens
        .iter()
        .find(|record| token_record_matches(record, &token_hash, now))
        .map(|record| record.scopes.clone()))
}

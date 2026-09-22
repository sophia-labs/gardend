use crate::loopback_client_token_types::{LoopbackClientTokenRecord, LoopbackClientTokenSummary};
use sha2::{Digest, Sha256};
use std::fmt::Write as _;

const DEFAULT_TOKEN_TTL_DAYS: u32 = 30;
const MAX_TOKEN_TTL_DAYS: u32 = 365;
const MILLIS_PER_DAY: u128 = 24 * 60 * 60 * 1000;

pub(super) fn expires_at_from_days(
    now: u128,
    expires_in_days: Option<u32>,
) -> Result<String, String> {
    let days = expires_in_days.unwrap_or(DEFAULT_TOKEN_TTL_DAYS);
    if !(1..=MAX_TOKEN_TTL_DAYS).contains(&days) {
        return Err(format!(
            "loopback client token expiry must be 1-{MAX_TOKEN_TTL_DAYS} days"
        ));
    }
    Ok((now + u128::from(days) * MILLIS_PER_DAY).to_string())
}

pub(super) fn hash_loopback_token(token: &str) -> String {
    let digest = Sha256::digest(token.as_bytes());
    let mut hex = String::with_capacity(64);
    for byte in digest {
        let _ = write!(&mut hex, "{byte:02x}");
    }
    format!("sha256:{hex}")
}

pub(super) fn token_record_matches(
    record: &LoopbackClientTokenRecord,
    token_hash: &str,
    now: u128,
) -> bool {
    !record.token_hash.is_empty()
        && record.token_hash == token_hash
        && record.revoked_at.is_none()
        && !token_is_expired(record, now)
}

pub(super) fn token_summary(
    record: &LoopbackClientTokenRecord,
    now: u128,
) -> LoopbackClientTokenSummary {
    LoopbackClientTokenSummary {
        token_id: record.token_id.clone(),
        label: record.label.clone(),
        grant_profile_id: record.grant_profile_id.clone(),
        scopes: record.scopes.clone(),
        created_at: record.created_at.clone(),
        expires_at: record.expires_at.clone(),
        revoked_at: record.revoked_at.clone(),
        active: record.revoked_at.is_none() && !token_is_expired(record, now),
    }
}

fn token_is_expired(record: &LoopbackClientTokenRecord, now: u128) -> bool {
    record
        .expires_at
        .parse::<u128>()
        .map(|expires_at| expires_at <= now)
        .unwrap_or(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_hash_does_not_store_plaintext() {
        let hash = hash_loopback_token("sophia-lb-secret");

        assert!(hash.starts_with("sha256:"));
        assert_eq!(hash, hash_loopback_token("sophia-lb-secret"));
        assert_ne!(hash, "sophia-lb-secret");
        assert!(!hash.contains("secret"));
    }

    #[test]
    fn token_expiry_days_are_bounded() {
        assert_eq!(
            expires_at_from_days(1_000, Some(1)).expect("one day"),
            (1_000 + MILLIS_PER_DAY).to_string()
        );
        assert!(expires_at_from_days(1_000, Some(0)).is_err());
        assert!(expires_at_from_days(1_000, Some(MAX_TOKEN_TTL_DAYS + 1)).is_err());
    }

    #[test]
    fn token_record_matching_requires_hash_active_and_not_expired() {
        let token_hash = hash_loopback_token("sophia-lb-secret");
        let mut record = LoopbackClientTokenRecord {
            token_id: "token-1".to_string(),
            label: "Client".to_string(),
            token_hash: token_hash.clone(),
            grant_profile_id: Some("read-only".to_string()),
            scopes: vec!["graphs.read".to_string()],
            created_at: "1000".to_string(),
            expires_at: "2000".to_string(),
            revoked_at: None,
        };

        assert!(token_record_matches(&record, &token_hash, 1999));
        assert!(!token_record_matches(&record, &token_hash, 2000));

        record.expires_at = "3000".to_string();
        record.revoked_at = Some("1500".to_string());
        assert!(!token_record_matches(&record, &token_hash, 1999));
    }

    #[test]
    fn token_summary_marks_expired_tokens_inactive() {
        let record = LoopbackClientTokenRecord {
            token_id: "token-1".to_string(),
            label: "Client".to_string(),
            token_hash: hash_loopback_token("sophia-lb-secret"),
            grant_profile_id: None,
            scopes: vec!["graphs.read".to_string()],
            created_at: "1000".to_string(),
            expires_at: "2000".to_string(),
            revoked_at: None,
        };

        assert!(token_summary(&record, 1999).active);
        assert!(!token_summary(&record, 2000).active);
    }
}

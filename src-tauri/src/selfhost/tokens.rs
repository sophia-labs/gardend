//! `gardend token create|list|revoke`: scoped, expiring client tokens for a
//! profile, through the same service the desktop app's token commands use.
//!
//! No running gardend needs telling: the store is a file in the profile that
//! every request re-reads (`resolve_loopback_client_token`), and writes are
//! atomic renames, so a token works (or stops working) on the next request.

use crate::loopback_client_token_service::{
    create_client_token_in, list_client_tokens_in, revoke_client_token_in,
};
use crate::loopback_client_token_types::{
    CreateLoopbackClientTokenInput, LoopbackClientTokenSummary, RevokeLoopbackClientTokenInput,
};
use std::path::Path;

/// The audit log's actor for tokens minted or revoked from the command line.
const CLI_ACTOR: &str = "gardend-cli";

/// The grant profiles a token can carry (`loopback_token_grants`). `member`
/// is the one to give agents and other people: the grant Cloudflare Access
/// members get.
pub(crate) const GRANT_PROFILES: &[&str] = &[
    "read-only",
    "authoring",
    "member",
    "rdf-admin",
    "session-all",
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TokenCreateArgs {
    pub label: String,
    pub grant: String,
    pub expires_days: Option<u32>,
    pub profile_dir: Option<String>,
    pub json: bool,
}

pub(crate) fn create(profile_dir: &Path, args: &TokenCreateArgs) -> Result<String, String> {
    if !profile_dir.is_dir() {
        return Err(format!(
            "no profile at {}: start it once with `gardend serve`, or name it with --profile-dir",
            profile_dir.display()
        ));
    }
    let created = create_client_token_in(
        profile_dir,
        CreateLoopbackClientTokenInput {
            label: Some(args.label.clone()),
            grant_profile_id: Some(args.grant.clone()),
            scopes: None,
            expires_in_days: args.expires_days,
        },
        CLI_ACTOR,
    )?;
    if args.json {
        return serde_json::to_string_pretty(&created)
            .map(|text| format!("{text}\n"))
            .map_err(|error| format!("serialize token: {error}"));
    }
    let record = &created.record;
    Ok(format!(
        "Created token {id} \"{label}\" ({grant}, expires {expires}).\n\
         The secret is shown this once; store it now:\n\n    {secret}\n\n\
         Send it as \"Authorization: Bearer <secret>\" to gardend's REST API and /mcp.\n\
         It works at once, also on a gardend that is already running.\n\
         Revoke it with: gardend token revoke {short}\n",
        id = record.token_id,
        label = record.label,
        grant = record
            .grant_profile_id
            .as_deref()
            .unwrap_or("custom scopes"),
        expires = millis_text(&record.expires_at),
        secret = created.token,
        short = short_id(&record.token_id),
    ))
}

pub(crate) fn list(profile_dir: &Path, json: bool) -> Result<String, String> {
    let tokens = list_client_tokens_in(profile_dir)?;
    if json {
        return serde_json::to_string_pretty(&tokens)
            .map(|text| format!("{text}\n"))
            .map_err(|error| format!("serialize tokens: {error}"));
    }
    if tokens.is_empty() {
        return Ok(format!(
            "No client tokens in {}. Create one with: gardend token create --label <name> --grant member\n",
            profile_dir.display()
        ));
    }
    let label_width = tokens
        .iter()
        .map(|token| token.label.chars().count())
        .max()
        .unwrap_or(5)
        .clamp(5, 32);
    let mut out = format!(
        "{:<8}  {:<label_width$}  {:<11}  {:<20}  STATUS\n",
        "ID", "LABEL", "PROFILE", "EXPIRES"
    );
    for token in &tokens {
        out.push_str(&format!(
            "{:<8}  {:<label_width$}  {:<11}  {:<20}  {}\n",
            short_id(&token.token_id),
            truncate(&token.label, label_width),
            token.grant_profile_id.as_deref().unwrap_or("custom"),
            millis_text(&token.expires_at),
            status(token),
        ));
    }
    Ok(out)
}

pub(crate) fn revoke(profile_dir: &Path, id: &str) -> Result<String, String> {
    let token_id = resolve_id(&list_client_tokens_in(profile_dir)?, id)?;
    let summary = revoke_client_token_in(
        profile_dir,
        RevokeLoopbackClientTokenInput { token_id },
        CLI_ACTOR,
    )?;
    Ok(format!(
        "Revoked {} \"{}\". It stops working on the next request, also on a running gardend.\n",
        summary.token_id, summary.label
    ))
}

/// A full id, or an unambiguous prefix of one (`token list` shows 8 chars).
fn resolve_id(tokens: &[LoopbackClientTokenSummary], id: &str) -> Result<String, String> {
    let id = id.trim();
    if id.is_empty() {
        return Err("give the id of the token to revoke (see `gardend token list`)".to_string());
    }
    if let Some(token) = tokens.iter().find(|token| token.token_id == id) {
        return Ok(token.token_id.clone());
    }
    let matches: Vec<_> = tokens
        .iter()
        .filter(|token| token.token_id.starts_with(id))
        .collect();
    match matches.as_slice() {
        [one] => Ok(one.token_id.clone()),
        [] => Err(format!("no token with id {id} (see `gardend token list`)")),
        many => Err(format!(
            "{id} matches {} tokens; give more of the id: {}",
            many.len(),
            many.iter()
                .map(|token| token.token_id.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        )),
    }
}

fn status(token: &LoopbackClientTokenSummary) -> &'static str {
    if token.revoked_at.is_some() {
        "revoked"
    } else if token.active {
        "active"
    } else {
        "expired"
    }
}

fn short_id(id: &str) -> &str {
    id.get(..8).unwrap_or(id)
}

fn truncate(text: &str, width: usize) -> String {
    if text.chars().count() <= width {
        return text.to_string();
    }
    let mut out: String = text.chars().take(width.saturating_sub(1)).collect();
    out.push('…');
    out
}

/// Epoch milliseconds (how the store keeps time) as a UTC date and time.
fn millis_text(millis: &str) -> String {
    millis
        .trim()
        .parse::<i64>()
        .ok()
        .and_then(chrono::DateTime::<chrono::Utc>::from_timestamp_millis)
        .map(|at| at.format("%Y-%m-%d %H:%M UTC").to_string())
        .unwrap_or_else(|| millis.to_string())
}

#[cfg(test)]
mod tests {
    use super::super::test_support::tempdir;
    use super::*;
    use crate::loopback_client_token_service::resolve_loopback_client_token_in;

    fn args(label: &str, grant: &str, json: bool) -> TokenCreateArgs {
        TokenCreateArgs {
            label: label.to_string(),
            grant: grant.to_string(),
            expires_days: Some(365),
            profile_dir: None,
            json,
        }
    }

    fn secret_of(json_output: &str) -> (String, String) {
        let value: serde_json::Value = serde_json::from_str(json_output).unwrap();
        (
            value["token"].as_str().unwrap().to_string(),
            value["record"]["tokenId"].as_str().unwrap().to_string(),
        )
    }

    #[test]
    fn create_list_revoke_against_a_real_profile() {
        let profile = tempdir("tokens");

        let (secret, id) =
            secret_of(&create(&profile, &args("lucy-phone", "authoring", true)).unwrap());
        assert!(secret.starts_with("sophia-lb-"));

        // The store keeps a hash, never the secret.
        let stored = std::fs::read_to_string(profile.join("loopback-client-tokens.json")).unwrap();
        assert!(!stored.contains(&secret));
        assert!(stored.contains("lucy-phone"));

        // What a running gardend's auth check resolves, read from the same file.
        let scopes = resolve_loopback_client_token_in(&profile, &secret)
            .unwrap()
            .expect("a fresh token resolves");
        assert!(scopes.iter().any(|scope| scope == "documents.write.crdt"));
        assert!(
            !scopes.iter().any(|scope| scope == "rdf.update"),
            "authoring is not rdf-admin"
        );

        let table = list(&profile, false).unwrap();
        assert!(table.contains("lucy-phone"));
        assert!(table.contains("authoring"));
        assert!(table.contains("active"));
        assert!(table.contains(&id[..8]));
        assert!(!table.contains(&secret));

        // Revoke by the short id `token list` prints.
        let revoked = revoke(&profile, &id[..8]).unwrap();
        assert!(revoked.contains(&id));
        assert!(resolve_loopback_client_token_in(&profile, &secret)
            .unwrap()
            .is_none());
        assert!(list(&profile, false).unwrap().contains("revoked"));

        // The audit log names the CLI and holds no secret.
        let audit = std::fs::read_to_string(profile.join("loopback-audit.jsonl")).unwrap();
        assert_eq!(audit.lines().count(), 2);
        assert!(audit.contains("\"actor\":\"gardend-cli\""));
        assert!(!audit.contains(&secret));
    }

    #[test]
    fn human_output_prints_the_secret_once() {
        let profile = tempdir("tokens-human");
        let out = create(&profile, &args("agent", "read-only", false)).unwrap();
        let secret_lines: Vec<_> = out
            .lines()
            .filter(|line| line.contains("sophia-lb-"))
            .collect();
        assert_eq!(secret_lines.len(), 1, "{out}");
        assert!(out.contains("(read-only, expires "));
    }

    #[test]
    fn bad_requests_are_refused() {
        let profile = tempdir("tokens-bad");
        assert!(create(&profile, &args("x", "superuser", false))
            .unwrap_err()
            .contains("unknown loopback grant profile"));
        let mut too_long = args("x", "read-only", false);
        too_long.expires_days = Some(366);
        assert!(create(&profile, &too_long)
            .unwrap_err()
            .contains("1-365 days"));
        assert!(
            create(&profile.join("missing"), &args("x", "read-only", false))
                .unwrap_err()
                .contains("no profile at")
        );
        assert!(revoke(&profile, "nope")
            .unwrap_err()
            .contains("no token with id"));
        assert!(list(&profile, false)
            .unwrap()
            .starts_with("No client tokens"));
    }

    #[test]
    fn a_member_token_holds_the_access_member_grant() {
        let profile = tempdir("tokens-member");
        let (secret, _) = secret_of(&create(&profile, &args("claude", "member", true)).unwrap());
        let scopes = resolve_loopback_client_token_in(&profile, &secret)
            .unwrap()
            .expect("a member token resolves");
        let held = |scope: &str| scopes.iter().any(|held| held == scope);
        for scope in [
            "mcp.tools.read",
            "mcp.tools.call",
            "documents.read",
            "documents.write.crdt",
            "rdf.query",
        ] {
            assert!(held(scope), "a member token holds {scope}");
        }
        for scope in [
            "documents.delete.crdt",
            "graphs.write",
            "rdf.load",
            "rdf.update",
            "time-travel.restore",
        ] {
            assert!(!held(scope), "a member token lacks {scope}");
        }
        let access_member = crate::loopback_token_grants::session_all_token_grant()
            .grant_profiles
            .into_iter()
            .find(|grant| grant.id == crate::loopback_self_host::MEMBER_GRANT)
            .expect("the grant Access members get")
            .scopes;
        assert_eq!(scopes, access_member);
        assert!(list(&profile, false).unwrap().contains("member"));
    }

    #[test]
    fn every_documented_grant_profile_mints() {
        let profile = tempdir("tokens-grants");
        for grant in GRANT_PROFILES {
            let (secret, _) = secret_of(&create(&profile, &args(grant, grant, true)).unwrap());
            assert!(resolve_loopback_client_token_in(&profile, &secret)
                .unwrap()
                .is_some());
        }
        assert_eq!(
            list_client_tokens_in(&profile).unwrap().len(),
            GRANT_PROFILES.len()
        );
    }

    #[test]
    fn dates_render_from_epoch_millis() {
        assert_eq!(millis_text("0"), "1970-01-01 00:00 UTC");
        assert_eq!(millis_text("1791763200000"), "2026-10-12 00:00 UTC");
        assert_eq!(millis_text("not-a-time"), "not-a-time");
    }
}

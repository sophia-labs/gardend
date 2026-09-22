//! SPEC-A acceptance cases 1, 3, 4, 5 — real compiled headless `gardend`,
//! real TCP, real disk manifest, real captured process stdout/stderr.
//!
//! Case 2 (Viewer-role, cell boundary ENABLED) is a separate, in-crate lib
//! test (`loopback_manifest_secret_redaction_cell_tests.rs`) because
//! standing up the full owner-scoped cell registry/lease-authority chain for
//! a subprocess is a different, heavier apparatus (see
//! `tests/cell_lease_durability.rs`) than this defect needs; that test
//! exercises the real axum router in-process with a real signed
//! `x-sophia-cell-lease` header instead.

mod support;

use reqwest::{Client, StatusCode};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::{fmt::Write as _, fs, time::Duration};
use support::gardend_process::{self as process, GardendConfig, LoopbackEndpoint, ScratchDir};

const READ_ONLY_TOKEN: &str = "manifest-redaction-integration-read-only-DO-NOT-LEAK-20260922";

fn hash_token(token: &str) -> String {
    let digest = Sha256::digest(token.as_bytes());
    let mut hex = String::with_capacity(64);
    for byte in digest {
        write!(&mut hex, "{byte:02x}").unwrap();
    }
    format!("sha256:{hex}")
}

/// Seed a named client token directly into `loopback-client-tokens.json` —
/// the exact wire format `loopback_client_token_store` reads, which the
/// running process re-reads from disk on every request (no in-memory
/// cache), so this works whether written before or after the process boots.
fn seed_read_only_client_token(profile_dir: &std::path::Path) {
    let store = serde_json::json!({
        "tokens": [{
            "tokenId": "manifest-redaction-read-only-fixture",
            "label": "manifest redaction read-only fixture",
            "tokenHash": hash_token(READ_ONLY_TOKEN),
            "grantProfileId": "read-only",
            "scopes": [
                "loopback.manifest.read",
                "graphs.read",
                "documents.read",
                "workspace.read",
            ],
            "createdAt": "0",
            "expiresAt": "99999999999999",
            "revokedAt": Value::Null,
        }]
    });
    fs::write(
        profile_dir.join("loopback-client-tokens.json"),
        serde_json::to_vec_pretty(&store).unwrap(),
    )
    .unwrap();
}

async fn get_manifest(port: u16, bearer: &str) -> (StatusCode, Vec<u8>) {
    let response = Client::new()
        .get(format!("http://127.0.0.1:{port}/manifest"))
        .bearer_auth(bearer)
        .send()
        .await
        .expect("GET /manifest");
    let status = response.status();
    let bytes = response
        .bytes()
        .await
        .expect("read /manifest body")
        .to_vec();
    (status, bytes)
}

fn assert_manifest_response_is_secret_free(body: &[u8], master_token: &str, context: &str) {
    let text = String::from_utf8_lossy(body).into_owned();
    assert!(
        !text.contains(master_token),
        "{context}: manifest response body contains the master bearer substring: {text}"
    );
    let value: Value = serde_json::from_slice(body)
        .unwrap_or_else(|error| panic!("{context}: manifest body was not JSON ({error}): {text}"));
    let object = value
        .as_object()
        .unwrap_or_else(|| panic!("{context}: manifest body was not a JSON object: {text}"));
    for secret_field in ["token", "pid", "manifestPath"] {
        assert!(
            !object.contains_key(secret_field),
            "{context}: manifest response must not carry `{secret_field}`: {text}"
        );
    }
    // Non-secret fields the parity checker asserts must still be present.
    for present_field in [
        "openapiUrl",
        "tokenScopeMode",
        "tokenAudience",
        "tokenStorage",
        "tokenScopes",
    ] {
        assert!(
            object.contains_key(present_field),
            "{context}: manifest response dropped non-secret field `{present_field}`: {text}"
        );
    }
}

#[tokio::test(flavor = "current_thread")]
async fn manifest_http_response_is_secret_free_for_named_read_only_token_and_for_owner_master_token(
) {
    let scratch = ScratchDir::new("manifest-redaction");
    let profile_dir = scratch.child("profile");
    let durable_dir = scratch.child("durable");
    let log_dir = scratch.child("logs");
    let graph_id = "manifest-redaction-fixture-20260922";
    process::prime_graph(&profile_dir, graph_id);

    let master_token = "manifest-redaction-integration-master-DO-NOT-LEAK-20260922".to_string();
    let (mut guard, _profile) = process::spawn_gardend(GardendConfig {
        profile_dir: profile_dir.clone(),
        durable_dir,
        graph_id: graph_id.to_string(),
        loopback_host: "127.0.0.1".to_string(),
        loopback_port: 0,
        extra_env: [("GARDEN_LOOPBACK_TOKEN".to_string(), master_token.clone())]
            .into_iter()
            .collect(),
        log_dir: log_dir.clone(),
        log_label: "gardend".to_string(),
    });

    let endpoint = LoopbackEndpoint::from_profile(&profile_dir, Duration::from_secs(30))
        .expect("gardend loopback endpoint came up");
    assert_eq!(
        endpoint.token, master_token,
        "harness read back the master token from disk"
    );

    // --- Acceptance case 4: disk loopback.json still contains the secret,
    // and the existing gardend harness (LoopbackEndpoint::from_profile,
    // consumed by tests/cell_lease_durability.rs and others) still parses
    // it and connects with no changes to that harness code. ---
    let disk_manifest: Value =
        serde_json::from_slice(&fs::read(profile_dir.join("loopback.json")).unwrap()).unwrap();
    assert_eq!(
        disk_manifest["token"], master_token,
        "disk loopback.json must still carry the raw token for the owner-only bootstrap channel"
    );
    assert!(
        disk_manifest.get("pid").and_then(Value::as_u64).is_some(),
        "disk loopback.json must still carry pid"
    );
    assert!(
        disk_manifest
            .get("manifestPath")
            .and_then(Value::as_str)
            .is_some(),
        "disk loopback.json must still carry manifestPath"
    );
    let (health_status, _) = endpoint.health().await;
    assert_eq!(
        health_status,
        StatusCode::OK,
        "existing harness health probe still works"
    );

    // Seed the named read-only client token now the profile dir exists.
    seed_read_only_client_token(&profile_dir);

    // --- Acceptance case 1: named read-only client token. ---
    let (status, body) = get_manifest(endpoint.port, READ_ONLY_TOKEN).await;
    assert_eq!(
        status,
        StatusCode::OK,
        "read-only token must be authorized for GET /manifest: {}",
        String::from_utf8_lossy(&body)
    );
    assert_manifest_response_is_secret_free(&body, &master_token, "named read-only token");

    // --- Acceptance case 3: Owner/master-token response is ALSO
    // secret-free — redaction is per-route, not per-principal. ---
    let (owner_status, owner_body) = get_manifest(endpoint.port, &master_token).await;
    assert_eq!(
        owner_status,
        StatusCode::OK,
        "master token must be authorized for GET /manifest: {}",
        String::from_utf8_lossy(&owner_body)
    );
    assert_manifest_response_is_secret_free(&owner_body, &master_token, "owner/master token");

    // --- Acceptance case 5: no log/error output anywhere in the real
    // process's captured stdout/stderr renders the master bearer. Scripted
    // grep gate over real captured process output (not a targeted logger
    // capture — env_logger writes straight to this process's stderr, so the
    // subprocess's own captured stream IS the ground truth). ---
    assert!(
        guard.is_running(),
        "gardend must still be running after the manifest requests"
    );
    let stdout = process::read_log(&log_dir, "gardend", "stdout");
    let stderr = process::read_log(&log_dir, "gardend", "stderr");
    assert!(
        !stdout.contains(&master_token),
        "gardend stdout rendered the master bearer:\n{stdout}"
    );
    assert!(
        !stderr.contains(&master_token),
        "gardend stderr rendered the master bearer:\n{stderr}"
    );

    drop(guard);
}

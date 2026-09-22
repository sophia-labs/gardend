use crate::app_runtime::AppHandle;
use crate::{
    clock::epoch_millis,
    original_file_types::{ImageAccessToken, ImageAccessTokenManifest, IMAGE_ACCESS_TOKEN_TTL_MS},
    paths::{existing_graph_dir, existing_graph_dir_no_heal, image_original_dir},
    storage::{read_json, write_secret_json},
};
use std::path::PathBuf;
use uuid::Uuid;

/// Healing path lookup — used only by `write_image_access_token`, which is
/// reached solely from the authenticated (`images.write`-scoped) upload
/// route, after the graph has already been created/self-healed by the
/// preceding `adopt_pending_image_file` call.
fn image_access_token_path(
    app: &AppHandle,
    graph_id: &str,
    image_id: &str,
) -> Result<PathBuf, String> {
    let graph_dir = existing_graph_dir(app, graph_id)?;
    Ok(image_original_dir(&graph_dir, image_id)?.join("access-token.json"))
}

/// Non-healing counterpart used by `image_access_token_matches` — the
/// anonymous, gateway-unauthenticated, query-signed image read/validation
/// path. A missing graph must 404/no-match here, never self-heal into
/// existence (F4c security review finding 1).
fn image_access_token_path_no_heal(
    app: &AppHandle,
    graph_id: &str,
    image_id: &str,
) -> Result<PathBuf, String> {
    let graph_dir = existing_graph_dir_no_heal(app, graph_id)?;
    Ok(image_original_dir(&graph_dir, image_id)?.join("access-token.json"))
}

pub(super) fn write_image_access_token(
    app: &AppHandle,
    graph_id: &str,
    image_id: &str,
) -> Result<ImageAccessToken, String> {
    let token_path = image_access_token_path(app, graph_id, image_id)?;
    let created_at_ms = epoch_millis();
    let expires_at = created_at_ms
        .saturating_add(IMAGE_ACCESS_TOKEN_TTL_MS)
        .to_string();
    let manifest = ImageAccessTokenManifest {
        token: Uuid::new_v4().simple().to_string(),
        created_at: created_at_ms.to_string(),
        expires_at,
    };
    write_secret_json(&token_path, &manifest)?;
    Ok(ImageAccessToken {
        token: manifest.token,
        expires_at: manifest.expires_at,
    })
}

pub(super) fn image_access_token_matches(
    app: &AppHandle,
    graph_id: &str,
    image_id: &str,
    candidate: Option<&String>,
    candidate_expires_at: Option<&String>,
) -> Result<bool, String> {
    let token_path = image_access_token_path_no_heal(app, graph_id, image_id)?;
    if !token_path.is_file() {
        return Ok(false);
    }
    let manifest = read_json::<ImageAccessTokenManifest>(&token_path)?;
    Ok(manifest.matches(candidate, candidate_expires_at, epoch_millis()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn image_access_token_matches_token_expiry_and_exp_query() {
        let manifest = ImageAccessTokenManifest {
            token: "secret".to_string(),
            created_at: "1000".to_string(),
            expires_at: "2000".to_string(),
        };
        let token = "secret".to_string();
        let expires_at = "2000".to_string();
        let wrong_expires_at = "2001".to_string();

        assert!(manifest.matches(Some(&token), None, 1999));
        assert!(manifest.matches(Some(&token), Some(&expires_at), 2000));
        assert!(!manifest.matches(Some(&"other".to_string()), Some(&expires_at), 1999));
        assert!(!manifest.matches(Some(&token), Some(&wrong_expires_at), 1999));
        assert!(!manifest.matches(Some(&token), Some(&expires_at), 2001));
        assert!(!manifest.matches(None, Some(&expires_at), 1999));
    }

    #[test]
    fn image_access_token_derives_legacy_expiry_from_created_at() {
        let manifest = ImageAccessTokenManifest {
            token: "legacy".to_string(),
            created_at: "1000".to_string(),
            expires_at: String::new(),
        };
        let token = "legacy".to_string();
        let expected_expiry = 1000 + IMAGE_ACCESS_TOKEN_TTL_MS;
        let expected_expiry_query = expected_expiry.to_string();

        assert_eq!(manifest.expires_at_ms(), Some(expected_expiry));
        assert!(manifest.matches(Some(&token), Some(&expected_expiry_query), expected_expiry));
        assert!(!manifest.matches(
            Some(&token),
            Some(&expected_expiry_query),
            expected_expiry + 1
        ));
    }
}

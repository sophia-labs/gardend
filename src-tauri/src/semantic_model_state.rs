use crate::app_runtime::AppHandle;
use crate::{
    clock::timestamp,
    model_setup_paths::{
        legacy_semantic_model_setup_path, semantic_model_config_path, semantic_model_setup_dir,
        semantic_model_setup_path,
    },
    semantic_model_catalog::{
        semantic_model_spec_by_id, SemanticModelSpec, SEMANTIC_MODEL_NOMIC_V2_MOE_ID,
    },
    semantic_model_remote::{
        remote_embeddings_endpoint, remote_pool_identity, REMOTE_POOL_MODEL_ID,
    },
    semantic_model_runtime::semantic_model_cache_path_hint,
    semantic_models::{
        SemanticModelSelectionConfig, SemanticModelSetupManifest, SEMANTIC_INDEX_SCHEMA_VERSION,
        SEMANTIC_MODEL_CACHE_POLICY,
    },
    storage::{create_dir_all, read_json, write_json},
};

pub(crate) fn read_semantic_model_config(
    app: &AppHandle,
) -> Result<SemanticModelSelectionConfig, String> {
    // Remote embeddings pool (headless cells): source model identity from the
    // pool probe instead of the omphalos constitution, and require no
    // constitution at all. Probed eagerly (fail-fast) so a down pool surfaces
    // at this same choke point, with the same call shape, as an absent
    // constitution does on the local path today.
    if let Some(endpoint) = remote_embeddings_endpoint() {
        let _identity = remote_pool_identity(&endpoint)?;
        // `selected_model_id` here is only the fixed routing key that sends
        // `semantic_model_spec_by_id` back into this same remote branch on
        // every subsequent lookup — it is deliberately NOT the pool's real
        // model id. The *effective* model identity (what actually lands in
        // `SemanticIndexManifest.model_id` and the search/reindex compat
        // checks) is `identity.pool_model_id` — the real `GET /info`
        // model id when available — resolved fresh from `spec.model_id` by
        // `semantic_model_spec_by_id(REMOTE_POOL_MODEL_ID)` at each call
        // site, not read off this struct. See `semantic_model_remote::
        // remote_pool_identity` for the swap-detection caveats.
        return Ok(SemanticModelSelectionConfig {
            schema_version: SEMANTIC_INDEX_SCHEMA_VERSION,
            selected_model_id: REMOTE_POOL_MODEL_ID.to_string(),
            batch_size: read_batch_size_override(app)?,
            updated_at: timestamp(),
        });
    }
    let selected_model_id = read_omphalos_semantic_model_id(app)?;
    Ok(SemanticModelSelectionConfig {
        schema_version: SEMANTIC_INDEX_SCHEMA_VERSION,
        selected_model_id,
        batch_size: read_batch_size_override(app)?,
        updated_at: timestamp(),
    })
}

fn read_batch_size_override(app: &AppHandle) -> Result<Option<usize>, String> {
    let path = semantic_model_config_path(app)?;
    if !path.is_file() {
        return Ok(None);
    }
    let config = read_json::<SemanticModelSelectionConfig>(&path)?;
    Ok((config.schema_version == SEMANTIC_INDEX_SCHEMA_VERSION)
        .then_some(config.batch_size)
        .flatten())
}

fn read_omphalos_semantic_model_id(app: &AppHandle) -> Result<String, String> {
    let selection = crate::omphalos::read_embedder_selection(app)?;
    semantic_model_id_from_embedder_selection(&selection)
}

fn semantic_model_id_from_embedder_selection(
    selection: &crate::omphalos::EmbedderSelection,
) -> Result<String, String> {
    let spec = semantic_model_spec_by_id(&selection.model_id)?;
    if spec.provider_id != selection.provider_id {
        return Err(format!(
            "omphalos embedder provider {} does not match catalog provider {} for {}",
            selection.provider_id, spec.provider_id, selection.model_id
        ));
    }
    if spec.dimensions != selection.dimensions {
        return Err(format!(
            "omphalos embedder dimensions {} do not match catalog dimensions {} for {}",
            selection.dimensions, spec.dimensions, selection.model_id
        ));
    }
    Ok(spec.model_id.to_string())
}

pub(crate) fn write_semantic_model_config(
    app: &AppHandle,
    model_id: &str,
    batch_size: Option<usize>,
) -> Result<(), String> {
    let spec = semantic_model_spec_by_id(model_id)?;
    let setup_dir = semantic_model_setup_dir(app)?;
    create_dir_all(&setup_dir)?;
    let batch_size = batch_size.map(|value| value.clamp(1, 64));
    write_json(
        &semantic_model_config_path(app)?,
        &SemanticModelSelectionConfig {
            schema_version: SEMANTIC_INDEX_SCHEMA_VERSION,
            selected_model_id: spec.model_id.to_string(),
            batch_size,
            updated_at: timestamp(),
        },
    )
    .map_err(Into::into)
}

pub(crate) fn semantic_model_effective_batch_size(
    spec: &SemanticModelSpec,
    config: &SemanticModelSelectionConfig,
) -> usize {
    config
        .batch_size
        .unwrap_or(spec.default_batch_size)
        .clamp(1, 64)
}

pub(crate) fn semantic_model_manifest(spec: &SemanticModelSpec) -> SemanticModelSetupManifest {
    SemanticModelSetupManifest {
        schema_version: SEMANTIC_INDEX_SCHEMA_VERSION,
        provider_id: spec.provider_id.to_string(),
        model_id: spec.model_id.to_string(),
        hf_repo: spec.hf_repo.to_string(),
        dimensions: spec.dimensions,
        max_tokens: spec.max_tokens,
        cache_policy: SEMANTIC_MODEL_CACHE_POLICY.to_string(),
        cache_path: semantic_model_cache_path_hint(),
        prepared_at: timestamp(),
    }
}

pub(crate) fn read_semantic_model_setup_manifest(
    app: &AppHandle,
    spec: &SemanticModelSpec,
) -> Result<Option<SemanticModelSetupManifest>, String> {
    let path = semantic_model_setup_path(app, spec.model_id)?;
    if !path.is_file() {
        if spec.model_id == SEMANTIC_MODEL_NOMIC_V2_MOE_ID {
            let legacy_path = legacy_semantic_model_setup_path(app)?;
            if legacy_path.is_file() {
                return read_json::<SemanticModelSetupManifest>(&legacy_path)
                    .map(Some)
                    .map_err(Into::into);
            }
        }
        return Ok(None);
    }
    read_json::<SemanticModelSetupManifest>(&path)
        .map(Some)
        .map_err(Into::into)
}

pub(crate) fn semantic_model_manifest_matches(
    spec: &SemanticModelSpec,
    manifest: &SemanticModelSetupManifest,
) -> bool {
    manifest.schema_version == SEMANTIC_INDEX_SCHEMA_VERSION
        && manifest.provider_id == spec.provider_id
        && manifest.model_id == spec.model_id
        && manifest.hf_repo == spec.hf_repo
        && manifest.dimensions == spec.dimensions
        && manifest.max_tokens == spec.max_tokens
}

pub(crate) fn write_semantic_model_setup_manifest(
    app: &AppHandle,
    spec: &SemanticModelSpec,
) -> Result<(), String> {
    let setup_dir = semantic_model_setup_dir(app)?;
    create_dir_all(&setup_dir)?;
    write_json(
        &semantic_model_setup_path(app, spec.model_id)?,
        &semantic_model_manifest(spec),
    )
    .map_err(Into::into)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::omphalos::EmbedderSelection;
    #[cfg(feature = "headless")]
    use crate::omphalos::DEFAULT_CONSTITUTION_TTL;
    #[cfg(feature = "headless")]
    use std::ffi::OsString;
    #[cfg(feature = "headless")]
    use std::time::{SystemTime, UNIX_EPOCH};

    #[cfg(feature = "headless")]
    struct EnvVarGuard {
        garden_profile_dir: Option<OsString>,
        sophia_omphalos: Option<OsString>,
        garden_embeddings_url: Option<OsString>,
    }

    #[cfg(feature = "headless")]
    impl EnvVarGuard {
        fn capture() -> Self {
            Self {
                garden_profile_dir: std::env::var_os("GARDEN_PROFILE_DIR"),
                sophia_omphalos: std::env::var_os("SOPHIA_OMPHALOS"),
                garden_embeddings_url: std::env::var_os(
                    crate::semantic_model_remote::REMOTE_EMBEDDINGS_ENV,
                ),
            }
        }
    }

    #[cfg(feature = "headless")]
    impl Drop for EnvVarGuard {
        fn drop(&mut self) {
            match &self.garden_profile_dir {
                Some(value) => std::env::set_var("GARDEN_PROFILE_DIR", value),
                None => std::env::remove_var("GARDEN_PROFILE_DIR"),
            }
            match &self.sophia_omphalos {
                Some(value) => std::env::set_var("SOPHIA_OMPHALOS", value),
                None => std::env::remove_var("SOPHIA_OMPHALOS"),
            }
            match &self.garden_embeddings_url {
                Some(value) => {
                    std::env::set_var(crate::semantic_model_remote::REMOTE_EMBEDDINGS_ENV, value)
                }
                None => std::env::remove_var(crate::semantic_model_remote::REMOTE_EMBEDDINGS_ENV),
            }
        }
    }

    #[cfg(feature = "headless")]
    fn temp_profile_dir(name: &str) -> std::path::PathBuf {
        let suffix = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        std::env::temp_dir().join(format!("garden-semantic-model-{name}-{suffix}"))
    }

    #[cfg(feature = "headless")]
    fn write_constitution(omphalos_dir: &std::path::Path, ttl: &str) {
        std::fs::create_dir_all(omphalos_dir).expect("create omphalos dir");
        std::fs::write(omphalos_dir.join("constitution.ttl"), ttl).expect("write constitution");
    }

    #[cfg(feature = "headless")]
    fn malformed_constitution_ttl() -> String {
        DEFAULT_CONSTITUTION_TTL.replace("nomos:dimensions 384 .", "nomos:dimensions \"wrong\" .")
    }

    #[cfg(feature = "headless")]
    fn custom_constitution_ttl() -> String {
        DEFAULT_CONSTITUTION_TTL.replace(
            "nomos:identityAnchor \"rdf-iri\"",
            "nomos:identityAnchor \"custom-rdf-iri\"",
        )
    }

    #[test]
    fn omphalos_embedder_selection_resolves_catalog_model() {
        let selected = semantic_model_id_from_embedder_selection(&EmbedderSelection {
            provider_id: "fastembed".to_string(),
            model_id: "fastembed/qdrant/bge-small-en-v1.5-onnx-q".to_string(),
            dimensions: 384,
        })
        .expect("omphalos selection resolves against catalog");

        assert_eq!(selected, "fastembed/qdrant/bge-small-en-v1.5-onnx-q");
    }

    #[cfg(feature = "headless")]
    #[test]
    fn fresh_profile_initializes_default_constitution_for_local_ai() {
        let _serial = crate::tauri_runtime::profile_env_serial()
            .lock()
            .expect("profile env mutex");
        let _env = EnvVarGuard::capture();
        let profile = temp_profile_dir("fresh-profile");
        std::env::set_var("GARDEN_PROFILE_DIR", &profile);
        std::env::remove_var("SOPHIA_OMPHALOS");
        let app = crate::tauri_runtime::build_mock_app_for_tests(false);

        crate::profile_service::ensure_profile(&app).expect("initialize fresh profile");
        write_semantic_model_config(&app, SEMANTIC_MODEL_NOMIC_V2_MOE_ID, Some(17))
            .expect("write conflicting legacy semantic model config");
        let config = read_semantic_model_config(&app).expect("read semantic config");

        assert_eq!(
            config.selected_model_id,
            "fastembed/qdrant/bge-small-en-v1.5-onnx-q"
        );
        assert_eq!(config.batch_size, Some(17));
        let store = crate::omphalos::open_omphalos(&app).expect("open initialized omphalos");
        let constitution =
            crate::omphalos::read_constitution(&store).expect("read default constitution");
        assert_eq!(constitution.schema_version, 2);
        assert_eq!(constitution.identity_anchor, "rdf-iri");
    }

    #[cfg(feature = "headless")]
    #[test]
    fn malformed_override_fails_without_corrupting_prior_constitution() {
        let _serial = crate::tauri_runtime::profile_env_serial()
            .lock()
            .expect("profile env mutex");
        let _env = EnvVarGuard::capture();
        let profile = temp_profile_dir("malformed-override");
        let omphalos_dir = profile.join("omphalos");
        std::env::set_var("GARDEN_PROFILE_DIR", &profile);
        std::env::remove_var("SOPHIA_OMPHALOS");
        let app = crate::tauri_runtime::build_mock_app_for_tests(false);

        crate::profile_service::ensure_profile(&app).expect("initialize valid profile");

        let store = crate::omphalos::open_omphalos(&app).expect("open omphalos");
        let before = crate::omphalos::read_constitution(&store).expect("read constituted omphalos");
        write_constitution(&omphalos_dir, &malformed_constitution_ttl());
        let error = crate::profile_service::ensure_profile(&app)
            .expect_err("malformed override must halt profile initialization");
        assert!(
            error.contains("SHACL"),
            "malformed constitution should fail validation loudly, got {error}"
        );
        assert!(
            read_semantic_model_config(&app).is_err(),
            "Local AI reads must continue surfacing the malformed override"
        );
        let after = crate::omphalos::read_constitution(&store)
            .expect("previous omphalos store remains readable");
        assert_eq!(after, before, "failed constitution must not alter store");
    }

    #[cfg(feature = "headless")]
    #[test]
    fn existing_store_constitution_is_not_replaced_by_default() {
        let _serial = crate::tauri_runtime::profile_env_serial()
            .lock()
            .expect("profile env mutex");
        let _env = EnvVarGuard::capture();
        let profile = temp_profile_dir("existing-store");
        std::env::set_var("GARDEN_PROFILE_DIR", &profile);
        std::env::remove_var("SOPHIA_OMPHALOS");
        let app = crate::tauri_runtime::build_mock_app_for_tests(false);

        let store = crate::omphalos::open_omphalos(&app).expect("open omphalos");
        crate::omphalos::constitute_turtle(&store, &custom_constitution_ttl())
            .expect("seed custom constitution directly");
        let before = crate::omphalos::read_constitution(&store).expect("read custom constitution");

        crate::profile_service::ensure_profile(&app).expect("initialize existing profile");

        let after =
            crate::omphalos::read_constitution(&store).expect("read preserved constitution");
        assert_eq!(after, before);
        assert_eq!(after.identity_anchor, "custom-rdf-iri");
        assert!(
            !profile.join("omphalos/constitution.ttl").exists(),
            "initialization must not write a default file over a store-only constitution"
        );
    }

    #[cfg(feature = "headless")]
    #[test]
    fn explicit_omphalos_constitution_is_used_without_overwriting_it() {
        let _serial = crate::tauri_runtime::profile_env_serial()
            .lock()
            .expect("profile env mutex");
        let _env = EnvVarGuard::capture();
        let explicit_profile = temp_profile_dir("explicit-profile");
        let explicit_omphalos = temp_profile_dir("explicit-omphalos");
        let custom = custom_constitution_ttl();
        write_constitution(&explicit_omphalos, &custom);
        let constitution_path = explicit_omphalos.join("constitution.ttl");
        let authored_before =
            std::fs::read(&constitution_path).expect("read authored constitution before init");

        std::env::set_var("GARDEN_PROFILE_DIR", &explicit_profile);
        std::env::set_var("SOPHIA_OMPHALOS", &explicit_omphalos);
        let explicit_app = crate::tauri_runtime::build_mock_app_for_tests(false);
        crate::profile_service::ensure_profile(&explicit_app)
            .expect("initialize profile with explicit omphalos");

        let store = crate::omphalos::open_omphalos(&explicit_app).expect("open explicit omphalos");
        let constitution =
            crate::omphalos::read_constitution(&store).expect("read explicit constitution");
        assert_eq!(constitution.identity_anchor, "custom-rdf-iri");
        assert_eq!(
            std::fs::read(&constitution_path).expect("read authored constitution after init"),
            authored_before,
            "initialization must never rewrite an authored constitution"
        );
        assert!(
            !explicit_profile.join("omphalos").exists(),
            "explicit SOPHIA_OMPHALOS should not create profile-local omphalos"
        );
    }

    #[cfg(feature = "headless")]
    #[test]
    fn read_semantic_model_config_remote_path_skips_omphalos_entirely() {
        let _serial = crate::tauri_runtime::profile_env_serial()
            .lock()
            .expect("profile env mutex");
        let _remote_serial =
            crate::semantic_model_remote::test_support::remote_embeddings_test_serial()
                .lock()
                .unwrap_or_else(|p| p.into_inner());
        let _env = EnvVarGuard::capture();
        let profile = temp_profile_dir("remote-skips-omphalos");
        std::env::set_var("GARDEN_PROFILE_DIR", &profile);
        std::env::remove_var("SOPHIA_OMPHALOS");

        let endpoint =
            crate::semantic_model_remote::test_support::spawn_fake_pool(|_method, path, body| {
                match path {
                    "/embed" => (
                        200,
                        crate::semantic_model_remote::test_support::embed_vectors_response(
                            crate::semantic_model_remote::test_support::embed_input_count(body),
                            4,
                        ),
                    ),
                    _ => (404, "not found".to_string()),
                }
            });
        std::env::set_var(
            crate::semantic_model_remote::REMOTE_EMBEDDINGS_ENV,
            &endpoint,
        );

        let app = crate::tauri_runtime::build_mock_app_for_tests(false);
        crate::profile_service::ensure_profile(&app)
            .expect("remote profile initializes without a local constitution");
        let config = read_semantic_model_config(&app)
            .expect("remote config resolves with no constitution present");

        assert_eq!(
            config.selected_model_id,
            crate::semantic_model_remote::REMOTE_POOL_MODEL_ID
        );
        assert!(
            !profile.join("omphalos").exists(),
            "remote path must never create a profile-local omphalos store"
        );
        let _ = std::fs::remove_dir_all(&profile);
    }

    #[cfg(feature = "headless")]
    #[test]
    fn read_semantic_model_config_remote_path_honors_batch_size_override_file() {
        let _serial = crate::tauri_runtime::profile_env_serial()
            .lock()
            .expect("profile env mutex");
        let _remote_serial =
            crate::semantic_model_remote::test_support::remote_embeddings_test_serial()
                .lock()
                .unwrap_or_else(|p| p.into_inner());
        let _env = EnvVarGuard::capture();
        let profile = temp_profile_dir("remote-batch-override");
        std::env::set_var("GARDEN_PROFILE_DIR", &profile);
        std::env::remove_var("SOPHIA_OMPHALOS");

        let endpoint =
            crate::semantic_model_remote::test_support::spawn_fake_pool(|_method, path, body| {
                match path {
                    "/embed" => (
                        200,
                        crate::semantic_model_remote::test_support::embed_vectors_response(
                            crate::semantic_model_remote::test_support::embed_input_count(body),
                            4,
                        ),
                    ),
                    _ => (404, "not found".to_string()),
                }
            });
        std::env::set_var(
            crate::semantic_model_remote::REMOTE_EMBEDDINGS_ENV,
            &endpoint,
        );

        let app = crate::tauri_runtime::build_mock_app_for_tests(false);
        write_semantic_model_config(
            &app,
            crate::semantic_model_remote::REMOTE_POOL_MODEL_ID,
            Some(9),
        )
        .expect("write batch size override against the remote sentinel");

        let config = read_semantic_model_config(&app)
            .expect("remote config resolves with an override file present");

        assert_eq!(
            config.selected_model_id,
            crate::semantic_model_remote::REMOTE_POOL_MODEL_ID
        );
        assert_eq!(config.batch_size, Some(9));
        let _ = std::fs::remove_dir_all(&profile);
    }

    #[cfg(feature = "headless")]
    #[test]
    fn read_semantic_model_config_local_path_unchanged_when_env_unset() {
        let _serial = crate::tauri_runtime::profile_env_serial()
            .lock()
            .expect("profile env mutex");
        let _remote_serial =
            crate::semantic_model_remote::test_support::remote_embeddings_test_serial()
                .lock()
                .unwrap_or_else(|p| p.into_inner());
        let _env = EnvVarGuard::capture();
        std::env::remove_var(crate::semantic_model_remote::REMOTE_EMBEDDINGS_ENV);
        let profile = temp_profile_dir("local-path-unchanged");
        let omphalos_dir = profile.join("omphalos");
        write_constitution(&omphalos_dir, DEFAULT_CONSTITUTION_TTL);
        std::env::set_var("GARDEN_PROFILE_DIR", &profile);
        std::env::remove_var("SOPHIA_OMPHALOS");

        let app = crate::tauri_runtime::build_mock_app_for_tests(false);
        let config =
            read_semantic_model_config(&app).expect("local path still resolves via omphalos");

        assert_eq!(
            config.selected_model_id,
            "fastembed/qdrant/bge-small-en-v1.5-onnx-q"
        );
        let _ = std::fs::remove_dir_all(&profile);
    }

    #[test]
    fn semantic_model_manifest_matches_current_spec() {
        let spec = semantic_model_spec_by_id(SEMANTIC_MODEL_NOMIC_V2_MOE_ID)
            .expect("nomic model spec should exist");
        let manifest = semantic_model_manifest(&spec);

        assert!(semantic_model_manifest_matches(&spec, &manifest));
    }
}

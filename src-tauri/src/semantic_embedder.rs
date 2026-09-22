use crate::app_runtime::AppHandle;
use crate::{
    app_error::{AppError, AppResult},
    semantic_embedder_backend::{embed_semantic_batch, load_semantic_embedder_backend},
    semantic_model_catalog::SemanticModelSpec,
    semantic_model_remote::remote_embeddings_endpoint,
    semantic_model_runtime::semantic_model_runtime_status,
    semantic_models::{
        read_semantic_model_config, semantic_model_catalog as semantic_model_catalog_with_loaded,
        semantic_model_spec_by_id, semantic_model_status as semantic_model_status_with_loaded,
        SemanticModelDescriptor, SemanticModelStatus,
    },
};
use std::sync::{Mutex, OnceLock};

static SEMANTIC_EMBEDDER: OnceLock<Mutex<Option<LoadedSemanticEmbedder>>> = OnceLock::new();

struct LoadedSemanticEmbedder {
    model_id: String,
    runtime_identity: SemanticEmbedderRuntimeIdentity,
    model: crate::semantic_embedder_backend::SemanticEmbedderBackend,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct SemanticEmbedderRuntimeIdentity {
    model_id: String,
    dimensions: usize,
    remote_endpoint: Option<String>,
}

fn semantic_embedder_runtime_identity(spec: &SemanticModelSpec) -> SemanticEmbedderRuntimeIdentity {
    SemanticEmbedderRuntimeIdentity {
        model_id: spec.model_id.to_string(),
        dimensions: spec.dimensions,
        remote_endpoint: remote_embeddings_endpoint(),
    }
}

pub(crate) trait SemanticEmbeddingProgress {
    fn check_cancelled(&self) -> AppResult<()>;

    fn report_embedding(
        &self,
        current: usize,
        total: usize,
        model_id: &str,
        dimensions: usize,
        batch_size: usize,
    ) -> AppResult<()>;
}

fn semantic_loaded_model_id() -> AppResult<Option<String>> {
    let state = SEMANTIC_EMBEDDER.get_or_init(|| Mutex::new(None));
    let loaded = state
        .lock()
        .map_err(|_| AppError::internal("semantic embedder lock was poisoned"))?;
    Ok(loaded.as_ref().map(|value| value.model_id.clone()))
}

pub(crate) fn semantic_model_status(app: &AppHandle) -> AppResult<SemanticModelStatus> {
    let loaded_model_id = semantic_loaded_model_id()?;
    semantic_model_status_with_loaded(app, loaded_model_id.as_deref()).map_err(AppError::internal)
}

pub(crate) fn semantic_model_catalog(app: &AppHandle) -> AppResult<Vec<SemanticModelDescriptor>> {
    let loaded_model_id = semantic_loaded_model_id()?;
    semantic_model_catalog_with_loaded(app, loaded_model_id.as_deref()).map_err(AppError::internal)
}

pub(crate) fn ensure_semantic_embedder(app: &AppHandle, allow_setup: bool) -> AppResult<()> {
    let config = read_semantic_model_config(app).map_err(AppError::storage)?;
    let spec =
        semantic_model_spec_by_id(&config.selected_model_id).map_err(AppError::validation)?;
    let runtime_identity = semantic_embedder_runtime_identity(&spec);
    let state = SEMANTIC_EMBEDDER.get_or_init(|| Mutex::new(None));
    {
        let loaded = state
            .lock()
            .map_err(|_| AppError::internal("semantic embedder lock was poisoned"))?;
        if loaded
            .as_ref()
            .is_some_and(|loaded| loaded.runtime_identity == runtime_identity)
        {
            return Ok(());
        }
    }

    let status = semantic_model_status(app)?;
    if !allow_setup && status.setup_required {
        return Err(AppError::validation(format!(
            "local embedding model {} is not prepared; run prepare_semantic_model first",
            spec.display_name
        )));
    }
    let (runtime_available, reason, _) = semantic_model_runtime_status(&spec);
    if !runtime_available {
        return Err(AppError::internal(reason.unwrap_or_else(|| {
            format!(
                "runtime for semantic model {} is unavailable",
                spec.display_name
            )
        })));
    }

    let model = load_semantic_embedder_backend(&spec, runtime_identity.remote_endpoint.as_deref())
        .map_err(AppError::internal)?;
    let mut loaded = state
        .lock()
        .map_err(|_| AppError::internal("semantic embedder lock was poisoned"))?;
    *loaded = Some(LoadedSemanticEmbedder {
        model_id: spec.model_id.to_string(),
        runtime_identity,
        model,
    });
    Ok(())
}

pub(crate) fn embed_texts(
    app: &AppHandle,
    texts: &[String],
    task_prefix: &str,
) -> AppResult<Vec<Vec<f32>>> {
    embed_texts_with_progress::<NoSemanticEmbeddingProgress>(app, texts, task_prefix, 64, None)
}

struct NoSemanticEmbeddingProgress;

impl SemanticEmbeddingProgress for NoSemanticEmbeddingProgress {
    fn check_cancelled(&self) -> AppResult<()> {
        Ok(())
    }

    fn report_embedding(
        &self,
        _current: usize,
        _total: usize,
        _model_id: &str,
        _dimensions: usize,
        _batch_size: usize,
    ) -> AppResult<()> {
        Ok(())
    }
}

pub(crate) fn embed_texts_with_progress<P: SemanticEmbeddingProgress>(
    app: &AppHandle,
    texts: &[String],
    task_prefix: &str,
    batch_size: usize,
    progress: Option<&P>,
) -> AppResult<Vec<Vec<f32>>> {
    if texts.is_empty() {
        return Ok(Vec::new());
    }
    ensure_semantic_embedder(app, false)?;
    let config = read_semantic_model_config(app).map_err(AppError::storage)?;
    let spec =
        semantic_model_spec_by_id(&config.selected_model_id).map_err(AppError::validation)?;
    let runtime_identity = semantic_embedder_runtime_identity(&spec);
    let embedder = SEMANTIC_EMBEDDER.get_or_init(|| Mutex::new(None));
    let mut vectors = Vec::new();
    let batch_size = batch_size.max(1);
    for chunk in texts.chunks(batch_size) {
        if let Some(progress) = progress {
            progress.check_cancelled()?;
        }
        let batch = chunk
            .iter()
            .map(|text| format!("{task_prefix}: {text}"))
            .collect::<Vec<_>>();
        let mut embedded = {
            let mut loaded = embedder
                .lock()
                .map_err(|_| AppError::internal("semantic embedder lock was poisoned"))?;
            let loaded = loaded
                .as_mut()
                .ok_or_else(|| AppError::internal("fastembed model was not initialized"))?;
            if loaded.runtime_identity != runtime_identity {
                return Err(AppError::internal(format!(
                    "loaded semantic runtime mismatch for active model {}",
                    spec.model_id
                )));
            }
            embed_semantic_batch(&mut loaded.model, &batch).map_err(AppError::internal)?
        };
        vectors.append(&mut embedded);
        if let Some(progress) = progress {
            progress.report_embedding(
                vectors.len(),
                texts.len(),
                spec.model_id,
                spec.dimensions,
                batch_size,
            )?;
        }
    }
    Ok(vectors)
}

/// Real (no-mock) coverage for the remote-embeddings setup-manifest bypass
/// (adversarial-review BLOCKER 1 on the remote-embeddings decouple): a fresh
/// remote cell has no local setup-manifest file on disk and must still be
/// able to embed, because there is no local model to "prepare" — the pool
/// does the compute. A real fake embeddings pool over a real TCP listener, a
/// real mock `AppHandle`, and a profile dir that never gets a
/// `semantic-model-setup/*.json` file written to it (prepare_semantic_model
/// is deliberately never called).
#[cfg(all(test, feature = "headless"))]
mod remote_setup_gate_tests {
    use super::*;
    use crate::semantic_model_remote::test_support::{
        embed_input_count, embed_vectors_response, remote_embeddings_test_serial, spawn_fake_pool,
    };
    use crate::semantic_model_remote::REMOTE_EMBEDDINGS_ENV;
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn profile_serial() -> &'static std::sync::Mutex<()> {
        crate::tauri_runtime::profile_env_serial()
    }

    fn temp_profile(name: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        std::env::temp_dir().join(format!("garden-semantic-embedder-{name}-{nanos}"))
    }

    #[test]
    fn ensure_semantic_embedder_succeeds_on_fresh_remote_cell_with_no_setup_manifest() {
        let _profile_serial = profile_serial().lock().unwrap_or_else(|p| p.into_inner());
        let _remote_serial = remote_embeddings_test_serial()
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        std::env::remove_var("SOPHIA_OMPHALOS");
        let profile = temp_profile("no-setup-manifest");
        std::env::set_var("GARDEN_PROFILE_DIR", &profile);

        let endpoint = spawn_fake_pool(|_method, path, body| match path {
            "/embed" => (200, embed_vectors_response(embed_input_count(body), 8)),
            "/health" => (200, "{}".to_string()),
            _ => (404, "not found".to_string()),
        });
        std::env::set_var(REMOTE_EMBEDDINGS_ENV, &endpoint);

        let app = crate::tauri_runtime::build_mock_app_for_tests(false);

        // No `semantic-model-setup/` dir exists anywhere under `profile` —
        // this is what "a fresh remote cell" means.
        let status = semantic_model_status(&app).expect("status resolves on a fresh remote cell");
        assert!(
            !status.setup_required,
            "remote pool readiness must not require a local setup manifest"
        );
        assert!(
            status.prepared,
            "remote path should report prepared without a manifest file on disk"
        );

        ensure_semantic_embedder(&app, false).expect(
            "ensure_semantic_embedder must not reject a fresh remote cell as an unprepared local model",
        );

        std::env::remove_var(REMOTE_EMBEDDINGS_ENV);
        let _ = std::fs::remove_dir_all(&profile);
    }

    #[test]
    fn semantic_embedder_reloads_for_remote_endpoint_identity_without_info() {
        let _profile_serial = profile_serial().lock().unwrap_or_else(|p| p.into_inner());
        let _remote_serial = remote_embeddings_test_serial()
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        std::env::remove_var("SOPHIA_OMPHALOS");
        let profile = temp_profile("endpoint-swap");
        std::env::set_var("GARDEN_PROFILE_DIR", &profile);
        let app = crate::tauri_runtime::build_mock_app_for_tests(false);

        let endpoint_a = spawn_fake_pool(|_method, path, body| match path {
            "/embed" => (200, embed_vectors_response(embed_input_count(body), 8)),
            _ => (404, "not found".to_string()),
        });
        std::env::set_var(REMOTE_EMBEDDINGS_ENV, &endpoint_a);
        let first = embed_texts(&app, &["first pool".to_string()], "query")
            .expect("first remote pool embeds");
        assert_eq!(first[0].len(), 8);
        assert_eq!(first[0][0], 0.125);

        let endpoint_b = spawn_fake_pool(|_method, path, body| match path {
            "/embed" => {
                let vectors = vec![vec![0.875_f32; 8]; embed_input_count(body).max(1)];
                (
                    200,
                    serde_json::to_string(&vectors)
                        .expect("serialize distinct same-dimension vectors"),
                )
            }
            _ => (404, "not found".to_string()),
        });
        std::env::set_var(REMOTE_EMBEDDINGS_ENV, &endpoint_b);
        let second = embed_texts(&app, &["second pool".to_string()], "query")
            .expect("same-dimension endpoint change reloads the process-wide embedder");
        assert_eq!(second[0].len(), 8);
        assert_eq!(second[0][0], 0.875);

        let endpoint_c = spawn_fake_pool(|_method, path, body| match path {
            "/embed" => (200, embed_vectors_response(embed_input_count(body), 32)),
            _ => (404, "not found".to_string()),
        });
        std::env::set_var(REMOTE_EMBEDDINGS_ENV, &endpoint_c);
        let third = embed_texts(&app, &["third pool".to_string()], "query")
            .expect("different-dimension endpoint change reloads the process-wide embedder");
        assert_eq!(third[0].len(), 32);

        std::env::remove_var(REMOTE_EMBEDDINGS_ENV);
        let _ = std::fs::remove_dir_all(&profile);
    }
}

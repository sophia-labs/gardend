use crate::app_runtime::AppHandle;
use crate::{
    model_setup_paths::{semantic_model_config_path, semantic_model_setup_path},
    semantic_model_catalog::{
        semantic_model_backend_label, semantic_model_spec_by_id, semantic_model_specs,
    },
    semantic_model_runtime::{semantic_model_cache_path_hint, semantic_model_runtime_status},
    semantic_model_state::{
        read_semantic_model_config, read_semantic_model_setup_manifest,
        semantic_model_effective_batch_size, semantic_model_manifest_matches,
    },
    semantic_models::SEMANTIC_MODEL_CACHE_POLICY,
    storage::display_path,
};

pub(crate) use crate::semantic_model_status_types::{SemanticModelDescriptor, SemanticModelStatus};

fn semantic_model_setup_required(prepared: bool, loaded: bool, runtime_available: bool) -> bool {
    (!prepared && !loaded) || !runtime_available
}

fn semantic_model_descriptor_status(
    runtime_available: bool,
    loaded: bool,
    prepared: bool,
) -> &'static str {
    if !runtime_available {
        "runtime-missing"
    } else if loaded {
        "loaded"
    } else if prepared {
        "available"
    } else {
        "setup-required"
    }
}

pub(crate) fn semantic_model_status(
    app: &AppHandle,
    loaded_model_id: Option<&str>,
) -> Result<SemanticModelStatus, String> {
    let config = read_semantic_model_config(app)?;
    let spec = semantic_model_spec_by_id(&config.selected_model_id)?;
    let setup_path = semantic_model_setup_path(app, spec.model_id)?;
    let manifest = read_semantic_model_setup_manifest(app, &spec)?;
    // Remote embeddings pool (headless cells): there is no local model to
    // prepare — the pool does the compute — so the local setup-manifest file
    // (written by prepare_semantic_model for ONNX/candle weights) is
    // irrelevant and will never exist on a fresh remote cell. Readiness on
    // this path is "the pool is reachable", which read_semantic_model_config
    // already probed (fail-fast) to resolve `spec` above, so by the time we
    // get here the pool is known-good. Mirrors the remote early-return in
    // semantic_model_runtime_status.
    let prepared = crate::semantic_model_remote::remote_embeddings_endpoint().is_some()
        || manifest
            .as_ref()
            .is_some_and(|manifest| semantic_model_manifest_matches(&spec, manifest));
    let prepared_at = if prepared {
        manifest.as_ref().map(|value| value.prepared_at.clone())
    } else {
        None
    };
    let loaded = loaded_model_id.is_some_and(|model_id| model_id == spec.model_id);
    let (runtime_available, reason, _) = semantic_model_runtime_status(&spec);

    Ok(SemanticModelStatus {
        provider_id: spec.provider_id.to_string(),
        model_id: spec.model_id.to_string(),
        display_name: spec.display_name.to_string(),
        hf_repo: spec.hf_repo.to_string(),
        dimensions: spec.dimensions,
        max_tokens: spec.max_tokens,
        backend: semantic_model_backend_label(&spec).to_string(),
        runtime: spec.runtime.to_string(),
        default_batch_size: spec.default_batch_size,
        effective_batch_size: semantic_model_effective_batch_size(&spec, &config),
        runtime_available,
        reason,
        prepared,
        loaded,
        setup_required: semantic_model_setup_required(prepared, loaded, runtime_available),
        config_path: display_path(&semantic_model_config_path(app)?),
        setup_path: display_path(&setup_path),
        cache_policy: SEMANTIC_MODEL_CACHE_POLICY.to_string(),
        cache_path: semantic_model_cache_path_hint(),
        prepared_at,
    })
}

pub(crate) fn semantic_model_catalog(
    app: &AppHandle,
    loaded_model_id: Option<&str>,
) -> Result<Vec<SemanticModelDescriptor>, String> {
    let config = read_semantic_model_config(app)?;
    semantic_model_specs()
        .into_iter()
        .map(|spec| {
            let manifest = read_semantic_model_setup_manifest(app, &spec)?;
            let prepared = manifest
                .as_ref()
                .is_some_and(|manifest| semantic_model_manifest_matches(&spec, manifest));
            let loaded = loaded_model_id.is_some_and(|model_id| model_id == spec.model_id);
            let (runtime_available, reason, setup_hint) = semantic_model_runtime_status(&spec);
            let selected = spec.model_id == config.selected_model_id;
            let prepared_at = if prepared {
                manifest.as_ref().map(|value| value.prepared_at.clone())
            } else {
                None
            };
            Ok(SemanticModelDescriptor {
                provider_id: spec.provider_id.to_string(),
                model_id: spec.model_id.to_string(),
                display_name: spec.display_name.to_string(),
                family: spec.family.to_string(),
                dimensions: spec.dimensions,
                max_tokens: spec.max_tokens,
                backend: semantic_model_backend_label(&spec).to_string(),
                task_prefixes: vec!["search_document".to_string(), "search_query".to_string()],
                hf_repo: Some(spec.hf_repo.to_string()),
                runtime: spec.runtime.to_string(),
                privacy: "local-profile".to_string(),
                cache_policy: SEMANTIC_MODEL_CACHE_POLICY.to_string(),
                cache_path: semantic_model_cache_path_hint(),
                setup_path: display_path(&semantic_model_setup_path(app, spec.model_id)?),
                setup_required: semantic_model_setup_required(prepared, loaded, runtime_available),
                prepared,
                loaded,
                prepared_at,
                default_batch_size: spec.default_batch_size,
                effective_batch_size: if selected {
                    semantic_model_effective_batch_size(&spec, &config)
                } else {
                    spec.default_batch_size
                },
                speed: spec.speed.to_string(),
                quality: spec.quality.to_string(),
                available: runtime_available && (prepared || loaded),
                selectable: true,
                reason,
                setup_hint,
                status: semantic_model_descriptor_status(runtime_available, loaded, prepared)
                    .to_string(),
                selected,
                recommended: spec.recommended,
                size_hint: spec.size_hint.to_string(),
                strengths: spec
                    .strengths
                    .iter()
                    .map(|value| value.to_string())
                    .collect(),
                limitations: spec
                    .limitations
                    .iter()
                    .map(|value| value.to_string())
                    .collect(),
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn semantic_model_setup_required_tracks_prepared_loaded_and_runtime() {
        assert!(semantic_model_setup_required(false, false, true));
        assert!(!semantic_model_setup_required(true, false, true));
        assert!(!semantic_model_setup_required(false, true, true));
        assert!(semantic_model_setup_required(true, true, false));
    }

    #[test]
    fn semantic_model_descriptor_status_reports_user_visible_state() {
        assert_eq!(
            semantic_model_descriptor_status(false, true, true),
            "runtime-missing"
        );
        assert_eq!(
            semantic_model_descriptor_status(true, true, false),
            "loaded"
        );
        assert_eq!(
            semantic_model_descriptor_status(true, false, true),
            "available"
        );
        assert_eq!(
            semantic_model_descriptor_status(true, false, false),
            "setup-required"
        );
    }
}

use crate::{
    semantic_model_catalog::{SemanticModelBackend, SemanticModelSpec},
    storage::display_path,
};
use std::path::{Path, PathBuf};

pub(crate) fn semantic_model_cache_path_hint() -> String {
    if let Ok(hf_home) = std::env::var("HF_HOME") {
        return display_path(Path::new(&hf_home));
    }
    if let Ok(home) = std::env::var("HOME") {
        return display_path(&Path::new(&home).join(".cache/huggingface/hub"));
    }
    "Hugging Face default cache".to_string()
}

pub(crate) fn semantic_onnx_runtime_dylib_path() -> Option<PathBuf> {
    if let Ok(path) = std::env::var("ORT_DYLIB_PATH") {
        let path = PathBuf::from(path);
        if path.is_file() {
            return Some(path);
        }
    }
    [
        "/usr/local/lib/libonnxruntime.dylib",
        "/usr/local/opt/onnxruntime/lib/libonnxruntime.dylib",
        "/opt/homebrew/lib/libonnxruntime.dylib",
        "/opt/homebrew/opt/onnxruntime/lib/libonnxruntime.dylib",
    ]
    .iter()
    .map(PathBuf::from)
    .find(|path| path.is_file())
}

pub(crate) fn semantic_model_runtime_status(
    spec: &SemanticModelSpec,
) -> (bool, Option<String>, Option<String>) {
    // Remote embeddings pool (headless cells): compute happens over HTTP,
    // so no local runtime (ONNX dylib, candle weights) is required for any
    // model. Reachability is verified at backend load.
    if let Some(endpoint) = crate::semantic_model_remote::remote_embeddings_endpoint() {
        return (
            true,
            Some(format!("remote embeddings pool at {endpoint}")),
            None,
        );
    }
    match &spec.backend {
        SemanticModelBackend::NomicV2Moe => (true, None, None),
        // Unreachable in practice: the remote check above always returns
        // first for a RemotePool spec. Kept so the match stays exhaustive.
        SemanticModelBackend::RemotePool => (true, None, None),
        SemanticModelBackend::FastembedText(_) => {
            if let Some(path) = semantic_onnx_runtime_dylib_path() {
                return (
                    true,
                    Some(format!(
                        "ONNX Runtime dylib found at {}",
                        display_path(&path)
                    )),
                    None,
                );
            }
            (
                false,
                Some(
                    "ONNX Runtime dylib is not configured; macOS x86_64 has no bundled prebuilt ORT in this crate"
                        .to_string(),
                ),
                Some(
                    "Install ONNX Runtime and set ORT_DYLIB_PATH to libonnxruntime.dylib before preparing this model."
                        .to_string(),
                ),
            )
        }
    }
}

pub(crate) fn ensure_semantic_onnx_runtime_available() -> Result<(), String> {
    let Some(path) = semantic_onnx_runtime_dylib_path() else {
        return Err(
            "ONNX Runtime dylib is not configured; install ONNX Runtime and set ORT_DYLIB_PATH"
                .to_string(),
        );
    };
    if std::env::var("ORT_DYLIB_PATH")
        .ok()
        .filter(|value| !value.is_empty())
        .is_none()
    {
        std::env::set_var("ORT_DYLIB_PATH", &path);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::semantic_model_catalog::{
        semantic_model_spec_by_id, SEMANTIC_MODEL_NOMIC_V2_MOE_ID,
    };

    #[test]
    fn nomic_runtime_is_available_without_onnx() {
        let spec = semantic_model_spec_by_id(SEMANTIC_MODEL_NOMIC_V2_MOE_ID)
            .expect("nomic model spec should exist");
        let (available, reason, setup_hint) = semantic_model_runtime_status(&spec);

        assert!(available);
        assert!(reason.is_none());
        assert!(setup_hint.is_none());
    }
}

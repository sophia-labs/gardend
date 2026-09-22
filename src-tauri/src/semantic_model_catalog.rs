use crate::semantic_model_remote::{
    remote_embeddings_endpoint, remote_pool_identity, RemotePoolIdentity, REMOTE_EMBEDDINGS_ENV,
    REMOTE_POOL_MODEL_ID,
};
use fastembed::EmbeddingModel;

pub(crate) const SEMANTIC_INDEX_PROVIDER_ID: &str = "fastembed";
pub(crate) const SEMANTIC_MODEL_BGE_SMALL_Q_ID: &str = "fastembed/qdrant/bge-small-en-v1.5-onnx-q";
const SEMANTIC_MODEL_BGE_SMALL_ID: &str = "fastembed/baai/bge-small-en-v1.5";
const SEMANTIC_MODEL_MINILM_ID: &str = "fastembed/sentence-transformers/all-MiniLM-L6-v2";
pub(crate) const SEMANTIC_MODEL_NOMIC_V2_MOE_ID: &str =
    "fastembed/nomic-ai/nomic-embed-text-v2-moe";
const SEMANTIC_MODEL_FASTEMBED_TEXT_BACKEND: &str = "fastembed-text-onnx";
const SEMANTIC_MODEL_NOMIC_BACKEND: &str = "fastembed-nomic-v2-moe-candle";
const SEMANTIC_MODEL_REMOTE_BACKEND: &str = "remote-embeddings-pool";
const SEMANTIC_MODEL_REMOTE_PROVIDER_ID: &str = "remote-embeddings-pool";

#[derive(Clone)]
pub(crate) enum SemanticModelBackend {
    FastembedText(EmbeddingModel),
    NomicV2Moe,
    RemotePool,
}

#[derive(Clone)]
pub(crate) struct SemanticModelSpec {
    pub(crate) provider_id: &'static str,
    pub(crate) model_id: &'static str,
    pub(crate) display_name: &'static str,
    pub(crate) family: &'static str,
    pub(crate) hf_repo: &'static str,
    pub(crate) dimensions: usize,
    pub(crate) max_tokens: usize,
    pub(crate) backend: SemanticModelBackend,
    pub(crate) runtime: &'static str,
    pub(crate) default_batch_size: usize,
    pub(crate) speed: &'static str,
    pub(crate) quality: &'static str,
    pub(crate) recommended: bool,
    pub(crate) size_hint: &'static str,
    pub(crate) strengths: &'static [&'static str],
    pub(crate) limitations: &'static [&'static str],
}

pub(crate) fn semantic_model_specs() -> Vec<SemanticModelSpec> {
    vec![
        SemanticModelSpec {
            provider_id: SEMANTIC_INDEX_PROVIDER_ID,
            model_id: SEMANTIC_MODEL_BGE_SMALL_Q_ID,
            display_name: "BGE Small EN v1.5 Quantized",
            family: "bge-small",
            hf_repo: "Qdrant/bge-small-en-v1.5-onnx-Q",
            dimensions: 384,
            max_tokens: 512,
            backend: SemanticModelBackend::FastembedText(EmbeddingModel::BGESmallENV15Q),
            runtime: "local-fastembed-onnx-cpu",
            default_batch_size: 4,
            speed: "fast",
            quality: "balanced",
            recommended: true,
            size_hint: "small static-quantized ONNX",
            strengths: &[
                "fast local indexing",
                "static quantization is safe for chunked indexes",
                "good default for desktop semantic search",
            ],
            limitations: &[
                "English-focused",
                "384-dimensional index is incompatible with Nomic indexes",
            ],
        },
        SemanticModelSpec {
            provider_id: SEMANTIC_INDEX_PROVIDER_ID,
            model_id: SEMANTIC_MODEL_BGE_SMALL_ID,
            display_name: "BGE Small EN v1.5",
            family: "bge-small",
            hf_repo: "Xenova/bge-small-en-v1.5",
            dimensions: 384,
            max_tokens: 512,
            backend: SemanticModelBackend::FastembedText(EmbeddingModel::BGESmallENV15),
            runtime: "local-fastembed-onnx-cpu",
            default_batch_size: 4,
            speed: "fast",
            quality: "balanced",
            recommended: false,
            size_hint: "small ONNX",
            strengths: &[
                "fast local indexing",
                "good recall for English notes",
                "no hosted embedding request",
            ],
            limitations: &[
                "English-focused",
                "384-dimensional index is incompatible with Nomic indexes",
            ],
        },
        SemanticModelSpec {
            provider_id: SEMANTIC_INDEX_PROVIDER_ID,
            model_id: SEMANTIC_MODEL_MINILM_ID,
            display_name: "All-MiniLM L6 v2",
            family: "minilm",
            hf_repo: "Qdrant/all-MiniLM-L6-v2-onnx",
            dimensions: 384,
            max_tokens: 256,
            backend: SemanticModelBackend::FastembedText(EmbeddingModel::AllMiniLML6V2),
            runtime: "local-fastembed-onnx-cpu",
            default_batch_size: 8,
            speed: "very fast",
            quality: "smoke-test",
            recommended: false,
            size_hint: "tiny ONNX",
            strengths: &["very fast indexing", "useful for fixture and smoke tests"],
            limitations: &[
                "lower retrieval quality than BGE/Nomic",
                "shorter default context window",
            ],
        },
        SemanticModelSpec {
            provider_id: SEMANTIC_INDEX_PROVIDER_ID,
            model_id: SEMANTIC_MODEL_NOMIC_V2_MOE_ID,
            display_name: "Nomic Embed Text V2 MoE",
            family: "nomic-embed",
            hf_repo: "nomic-ai/nomic-embed-text-v2-moe",
            dimensions: 768,
            max_tokens: 512,
            backend: SemanticModelBackend::NomicV2Moe,
            runtime: "local-fastembed-candle-cpu",
            default_batch_size: if cfg!(all(target_os = "macos", target_arch = "x86_64")) {
                1
            } else {
                8
            },
            speed: "slow",
            quality: "quality",
            recommended: false,
            size_hint: "large Candle model",
            strengths: &[
                "stronger semantic quality",
                "768-dimensional block vectors",
                "no hosted embedding request",
            ],
            limitations: &[
                "CPU indexing can be very slow on Mac Intel",
                "batch cancellation only occurs between embedding calls",
            ],
        },
    ]
}

pub(crate) fn semantic_model_spec_by_id(model_id: &str) -> Result<SemanticModelSpec, String> {
    if model_id == REMOTE_POOL_MODEL_ID {
        let endpoint = remote_embeddings_endpoint().ok_or_else(|| {
            format!("semantic model id {REMOTE_POOL_MODEL_ID} requires {REMOTE_EMBEDDINGS_ENV} to be set")
        })?;
        let identity = remote_pool_identity(&endpoint)?;
        return Ok(remote_pool_spec(&identity));
    }
    semantic_model_specs()
        .into_iter()
        .find(|spec| spec.model_id == model_id)
        .ok_or_else(|| format!("unknown semantic model id: {model_id}"))
}

/// Synthesizes a `SemanticModelSpec` for the remote embeddings pool from a
/// probed `RemotePoolIdentity`. Only `model_id`/`dimensions` are load-bearing
/// for any downstream comparison/branch (see the remote-embeddings decouple
/// spec §4c) — the rest exist purely because `SemanticModelSpec` has no
/// `Option` fields.
fn remote_pool_spec(identity: &RemotePoolIdentity) -> SemanticModelSpec {
    SemanticModelSpec {
        provider_id: SEMANTIC_MODEL_REMOTE_PROVIDER_ID,
        model_id: identity.pool_model_id,
        display_name: "Remote Embeddings Pool",
        family: "remote-embeddings-pool",
        hf_repo: "",
        dimensions: identity.dimensions,
        max_tokens: 512,
        backend: SemanticModelBackend::RemotePool,
        runtime: "remote-embeddings-pool-http",
        default_batch_size: 32,
        speed: "network",
        quality: "pool-managed",
        recommended: false,
        size_hint: "hosted embeddings pool",
        strengths: &[
            "no local model download or ONNX/candle runtime required",
            "model identity and quality are managed by the pool operator",
        ],
        limitations: &[
            "requires network connectivity to the configured pool",
            "a mid-process pool model swap surfaces as a mismatch until the cell recycles",
        ],
    }
}

pub(crate) fn semantic_model_backend_label(spec: &SemanticModelSpec) -> &'static str {
    match &spec.backend {
        SemanticModelBackend::FastembedText(_) => SEMANTIC_MODEL_FASTEMBED_TEXT_BACKEND,
        SemanticModelBackend::NomicV2Moe => SEMANTIC_MODEL_NOMIC_BACKEND,
        SemanticModelBackend::RemotePool => SEMANTIC_MODEL_REMOTE_BACKEND,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    #[test]
    fn semantic_model_catalog_ids_are_unique() {
        let specs = semantic_model_specs();
        let ids = specs
            .iter()
            .map(|spec| spec.model_id)
            .collect::<BTreeSet<_>>();

        assert_eq!(ids.len(), specs.len());
        assert!(ids.contains(SEMANTIC_MODEL_BGE_SMALL_Q_ID));
        assert!(ids.contains(SEMANTIC_MODEL_NOMIC_V2_MOE_ID));
    }

    #[test]
    fn semantic_model_spec_by_id_resolves_remote_sentinel_when_env_set() {
        use crate::semantic_model_remote::test_support::{
            embed_input_count, embed_vectors_response, remote_embeddings_test_serial,
            spawn_fake_pool,
        };

        let _serial = remote_embeddings_test_serial()
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        let endpoint = spawn_fake_pool(|_method, path, body| match path {
            "/embed" => (200, embed_vectors_response(embed_input_count(body), 4)),
            _ => (404, "not found".to_string()),
        });
        std::env::set_var(REMOTE_EMBEDDINGS_ENV, &endpoint);

        let result = semantic_model_spec_by_id(REMOTE_POOL_MODEL_ID);

        std::env::remove_var(REMOTE_EMBEDDINGS_ENV);

        let spec = result.expect("remote sentinel resolves when the env var is set");
        assert_eq!(spec.dimensions, 4);
        assert!(matches!(spec.backend, SemanticModelBackend::RemotePool));
    }

    #[test]
    fn semantic_model_spec_by_id_errors_when_sentinel_requested_without_env() {
        use crate::semantic_model_remote::test_support::remote_embeddings_test_serial;

        let _serial = remote_embeddings_test_serial()
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        std::env::remove_var(REMOTE_EMBEDDINGS_ENV);

        let error = match semantic_model_spec_by_id(REMOTE_POOL_MODEL_ID) {
            Ok(_) => panic!("remote sentinel unexpectedly resolved without its endpoint"),
            Err(error) => error,
        };

        assert!(error.contains(REMOTE_EMBEDDINGS_ENV), "got: {error}");
    }
}

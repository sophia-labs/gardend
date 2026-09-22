use crate::{
    semantic_model_catalog::{SemanticModelBackend, SemanticModelSpec},
    semantic_model_runtime::ensure_semantic_onnx_runtime_available,
};
use candle_core::{DType, Device};
use fastembed::{NomicV2MoeTextEmbedding, TextEmbedding, TextInitOptions};

pub(crate) enum SemanticEmbedderBackend {
    FastembedText(TextEmbedding),
    NomicV2Moe(NomicV2MoeTextEmbedding),
    Remote(RemoteEmbedder),
}

pub(crate) struct RemoteEmbedder {
    endpoint: String,
    dimensions: usize,
    agent: ureq::Agent,
}

impl RemoteEmbedder {
    fn connect(endpoint: String, spec: &SemanticModelSpec) -> Result<Self, String> {
        let agent = ureq::Agent::new_with_defaults();
        let health_url = format!("{endpoint}/health");
        agent
            .get(&health_url)
            .call()
            .map_err(|error| format!("embeddings pool not reachable at {health_url}: {error}"))?;
        Ok(Self {
            endpoint,
            dimensions: spec.dimensions,
            agent,
        })
    }

    fn embed(&self, batch: &[String]) -> Result<Vec<Vec<f32>>, String> {
        let url = format!("{}/embed", self.endpoint);
        let mut response = self
            .agent
            .post(&url)
            .send_json(serde_json::json!({ "inputs": batch, "truncate": true }))
            .map_err(|error| format!("embeddings pool request failed ({url}): {error}"))?;
        let vectors: Vec<Vec<f32>> = response
            .body_mut()
            .read_json()
            .map_err(|error| format!("embeddings pool returned invalid JSON: {error}"))?;
        if vectors.len() != batch.len() {
            return Err(format!(
                "embeddings pool returned {} vectors for {} inputs",
                vectors.len(),
                batch.len()
            ));
        }
        if let Some(bad) = vectors.iter().find(|v| v.len() != self.dimensions) {
            return Err(format!(
                "embeddings pool returned {}-dim vectors, expected {} — pool model mismatch",
                bad.len(),
                self.dimensions
            ));
        }
        Ok(vectors)
    }
}

pub(crate) fn load_semantic_embedder_backend(
    spec: &SemanticModelSpec,
    remote_endpoint: Option<&str>,
) -> Result<SemanticEmbedderBackend, String> {
    // Remote pool takes precedence over local backends when configured
    // (headless cells). Health-probed here so the existing prepare/status
    // flows report failures at the same point local model loads would. The
    // caller supplies the endpoint it used to derive the loaded-runtime
    // identity so an env change cannot bind a different backend between the
    // cache check and this load.
    if let Some(endpoint) = remote_endpoint {
        return Ok(SemanticEmbedderBackend::Remote(RemoteEmbedder::connect(
            endpoint.to_string(),
            spec,
        )?));
    }
    match spec.backend.clone() {
        SemanticModelBackend::FastembedText(model_name) => {
            ensure_semantic_onnx_runtime_available()?;
            let options = TextInitOptions::new(model_name)
                .with_max_length(spec.max_tokens)
                .with_show_download_progress(false);
            Ok(SemanticEmbedderBackend::FastembedText(
                TextEmbedding::try_new(options)
                    .map_err(|error| format!("initialize fastembed ONNX model: {error}"))?,
            ))
        }
        SemanticModelBackend::NomicV2Moe => {
            let device = Device::Cpu;
            Ok(SemanticEmbedderBackend::NomicV2Moe(
                NomicV2MoeTextEmbedding::from_hf(
                    spec.hf_repo,
                    &device,
                    DType::F32,
                    spec.max_tokens,
                )
                .map_err(|error| format!("initialize fastembed Nomic model: {error}"))?,
            ))
        }
        // Unreachable in practice: the remote early-return above always
        // intercepts a RemotePool spec before this match runs. Kept so the
        // match stays exhaustive and self-documenting.
        SemanticModelBackend::RemotePool => Err(
            "semantic model backend RemotePool reached the local loader; GARDEN_EMBEDDINGS_URL was unset after a remote spec was resolved".to_string(),
        ),
    }
}

pub(crate) fn embed_semantic_batch(
    model: &mut SemanticEmbedderBackend,
    batch: &[String],
) -> Result<Vec<Vec<f32>>, String> {
    match model {
        SemanticEmbedderBackend::FastembedText(model) => model
            .embed(batch, Some(batch.len()))
            .map_err(|error| format!("generate fastembed ONNX embeddings: {error}")),
        SemanticEmbedderBackend::NomicV2Moe(model) => model
            .embed(batch)
            .map_err(|error| format!("generate fastembed Nomic embeddings: {error}")),
        SemanticEmbedderBackend::Remote(remote) => remote.embed(batch),
    }
}

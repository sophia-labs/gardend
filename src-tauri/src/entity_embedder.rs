//! The embedder of the RDF entity index: ONNX Runtime driven directly, so its
//! intra-op threads are a budget (`GARDEN_EMBED_THREADS`) rather than every core
//! of the machine (fastembed 5 pins them to `available_parallelism` with no
//! knob). Same ONNX files, tokenizer settings and pooling as fastembed: the
//! spike measured identical vectors (cos 1.000000).
//!
//! Each model carries its own prefixes and a pinned identity. E5 needs
//! `query: ` / `passage: `; an index records the model and prefixes it was built
//! with, and a query against it uses exactly those.

use fastembed::{EmbeddingModel, Pooling, TextEmbedding, TextInitOptions};
use serde::Serialize;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

/// The default model of the entity index: multilingual, 384 dimensions.
pub(crate) const DEFAULT_ENTITY_MODEL: &str = "intfloat/multilingual-e5-small";

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct EntityModelSpec {
    pub(crate) model_id: &'static str,
    pub(crate) dimensions: usize,
    pub(crate) query_prefix: &'static str,
    pub(crate) passage_prefix: &'static str,
    #[serde(skip)]
    pub(crate) model: EmbeddingModel,
}

pub(crate) fn entity_model_spec(model_id: &str) -> Result<EntityModelSpec, String> {
    let (model_id, dimensions, model) = match model_id.trim() {
        "" | DEFAULT_ENTITY_MODEL | "multilingual-e5-small" | "e5-small" => {
            (DEFAULT_ENTITY_MODEL, 384, EmbeddingModel::MultilingualE5Small)
        }
        "intfloat/multilingual-e5-base" | "multilingual-e5-base" | "e5-base" => {
            ("intfloat/multilingual-e5-base", 768, EmbeddingModel::MultilingualE5Base)
        }
        other => {
            return Err(format!(
                "unknown entity index model {other:?}: use {DEFAULT_ENTITY_MODEL} (default) or intfloat/multilingual-e5-base"
            ))
        }
    };
    Ok(EntityModelSpec {
        model_id,
        dimensions,
        query_prefix: "query: ",
        passage_prefix: "passage: ",
        model,
    })
}

/// Threads for one embedding session: `GARDEN_EMBED_THREADS`, else the CPUs
/// this process may use (cgroup quotas included), at most 4.
pub(crate) fn embed_thread_budget(requested: Option<usize>) -> usize {
    requested
        .or_else(|| {
            std::env::var("GARDEN_EMBED_THREADS")
                .ok()
                .and_then(|value| value.trim().parse::<usize>().ok())
        })
        .unwrap_or_else(|| {
            std::thread::available_parallelism()
                .map(|n| n.get())
                .unwrap_or(1)
                .min(4)
        })
        .clamp(1, 64)
}

pub(crate) struct EntityEmbedder {
    pub(crate) spec: EntityModelSpec,
    pub(crate) threads: usize,
    session: ort::session::Session,
    tokenizer: tokenizers::Tokenizer,
    pooling: Pooling,
    needs_token_types: bool,
}

const MAX_TOKENS: usize = 256;

/// Where fastembed puts (and finds) model files: `HF_HOME` when it is set
/// (fastembed's `pull_from_hf` prefers it over its own cache directory), else
/// `FASTEMBED_CACHE_DIR`, else `.fastembed_cache`.
fn model_cache_dir() -> PathBuf {
    model_cache_root(std::env::var("HF_HOME").ok(), fastembed::get_cache_dir())
}

fn model_cache_root(hf_home: Option<String>, fastembed_dir: String) -> PathBuf {
    hf_home
        .filter(|dir| !dir.trim().is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(fastembed_dir))
}

fn snapshot_dir(cache: &Path, model_code: &str) -> Option<PathBuf> {
    let dir = cache
        .join(format!("models--{}", model_code.replace('/', "--")))
        .join("snapshots");
    std::fs::read_dir(&dir)
        .ok()?
        .flatten()
        .map(|entry| entry.path())
        .find(|path| path.is_dir())
}

/// Same padding and truncation as fastembed's (private) tokenizer loader.
fn load_tokenizer(dir: &Path, max_len: usize) -> Result<tokenizers::Tokenizer, String> {
    let read = |name: &str| std::fs::read(dir.join(name)).map_err(|error| format!("{name}: {error}"));
    let config: serde_json::Value =
        serde_json::from_slice(&read("config.json")?).map_err(|error| format!("config.json: {error}"))?;
    let tokenizer_config: serde_json::Value = serde_json::from_slice(&read("tokenizer_config.json")?)
        .map_err(|error| format!("tokenizer_config.json: {error}"))?;
    let mut tokenizer = tokenizers::Tokenizer::from_bytes(read("tokenizer.json")?)
        .map_err(|error| format!("tokenizer.json: {error}"))?;
    let model_max = tokenizer_config["model_max_length"]
        .as_f64()
        .map(|value| value.min(1_000_000.0) as usize)
        .unwrap_or(512);
    tokenizer.with_padding(Some(tokenizers::PaddingParams {
        strategy: tokenizers::PaddingStrategy::BatchLongest,
        pad_token: tokenizer_config["pad_token"].as_str().unwrap_or("<pad>").to_string(),
        pad_id: config["pad_token_id"].as_u64().unwrap_or(0) as u32,
        ..Default::default()
    }));
    tokenizer
        .with_truncation(Some(tokenizers::TruncationParams {
            max_length: max_len.min(model_max),
            ..Default::default()
        }))
        .map_err(|error| format!("tokenizer truncation: {error}"))?;
    Ok(tokenizer)
}

impl EntityEmbedder {
    pub(crate) fn load(model_id: &str, threads: usize) -> Result<Self, String> {
        crate::semantic_model_runtime::ensure_semantic_onnx_runtime_available()?;
        let spec = entity_model_spec(model_id)?;
        let info = TextEmbedding::get_model_info(&spec.model)
            .map_err(|error| format!("entity model info: {error}"))?
            .clone();
        let cache = model_cache_dir();
        let mut dir = snapshot_dir(&cache, &info.model_code);
        if dir.as_ref().map_or(true, |dir| !dir.join(&info.model_file).is_file()) {
            // fastembed fetches the files once (Hugging Face, into its cache); its
            // own session is dropped at once.
            TextEmbedding::try_new(
                TextInitOptions::new(spec.model.clone())
                    .with_cache_dir(cache.clone())
                    .with_show_download_progress(false),
            )
            .map_err(|error| format!("download {}: {error}", spec.model_id))?;
            dir = snapshot_dir(&cache, &info.model_code);
        }
        let dir = dir.ok_or_else(|| format!("model files for {} not found in {}", spec.model_id, cache.display()))?;
        let tokenizer = load_tokenizer(&dir, MAX_TOKENS)?;
        let session = ort::session::Session::builder()
            .map_err(|error| format!("onnx session: {error}"))?
            .with_optimization_level(ort::session::builder::GraphOptimizationLevel::Level3)
            .map_err(|error| format!("onnx session: {error}"))?
            .with_intra_threads(threads)
            .map_err(|error| format!("onnx session: {error}"))?
            .with_inter_threads(1)
            .map_err(|error| format!("onnx session: {error}"))?
            .commit_from_file(dir.join(&info.model_file))
            .map_err(|error| format!("onnx session for {}: {error}", spec.model_id))?;
        let needs_token_types = session
            .inputs()
            .iter()
            .any(|input| input.name() == "token_type_ids");
        let pooling = TextEmbedding::get_default_pooling_method(&spec.model).unwrap_or(Pooling::Mean);
        Ok(Self {
            spec,
            threads,
            session,
            tokenizer,
            pooling,
            needs_token_types,
        })
    }

    fn run(&mut self, texts: &[String]) -> Result<Vec<Vec<f32>>, String> {
        if texts.is_empty() {
            return Ok(Vec::new());
        }
        let encodings = self
            .tokenizer
            .encode_batch(texts.to_vec(), true)
            .map_err(|error| format!("tokenize: {error}"))?;
        let batch = encodings.len();
        let len = encodings[0].len();
        let mut ids = Vec::with_capacity(batch * len);
        let mut mask = Vec::with_capacity(batch * len);
        let mut types = Vec::with_capacity(batch * len);
        for encoding in &encodings {
            ids.extend(encoding.get_ids().iter().map(|&x| x as i64));
            mask.extend(encoding.get_attention_mask().iter().map(|&x| x as i64));
            types.extend(encoding.get_type_ids().iter().map(|&x| x as i64));
        }
        let shape = vec![batch as i64, len as i64];
        let tensor = |data: Vec<i64>| {
            ort::value::Tensor::from_array((shape.clone(), data)).map_err(|error| format!("tensor: {error}"))
        };
        let mut inputs = ort::inputs![
            "input_ids" => tensor(ids)?,
            "attention_mask" => tensor(mask.clone())?,
        ];
        if self.needs_token_types {
            inputs.push(("token_type_ids".into(), tensor(types)?.into()));
        }
        let outputs = self.session.run(inputs).map_err(|error| format!("onnx run: {error}"))?;
        let names: Vec<String> = outputs.keys().map(|key| key.to_string()).collect();
        let key = ["text_embeds", "last_hidden_state", "sentence_embedding"]
            .iter()
            .find(|candidate| names.iter().any(|name| name == *candidate))
            .map(|name| name.to_string())
            .or_else(|| names.first().cloned())
            .ok_or_else(|| "onnx model returned no output".to_string())?;
        let (shape, data) = outputs[key.as_str()]
            .try_extract_tensor::<f32>()
            .map_err(|error| format!("onnx output: {error}"))?;
        let dims: Vec<usize> = shape.iter().map(|&d| d as usize).collect();
        let mut vectors = Vec::with_capacity(batch);
        if dims.len() == 2 {
            for i in 0..batch {
                vectors.push(normalize(data[i * dims[1]..(i + 1) * dims[1]].to_vec()));
            }
        } else if dims.len() == 3 {
            let (seq, hidden) = (dims[1], dims[2]);
            for i in 0..batch {
                let mut vector = vec![0f32; hidden];
                if matches!(self.pooling, Pooling::Cls) {
                    vector.copy_from_slice(&data[i * seq * hidden..i * seq * hidden + hidden]);
                } else {
                    let mut count = 0f32;
                    for t in 0..seq {
                        if mask[i * len + t] == 0 {
                            continue;
                        }
                        count += 1.0;
                        let row = &data[(i * seq + t) * hidden..(i * seq + t + 1) * hidden];
                        for (acc, x) in vector.iter_mut().zip(row) {
                            *acc += x;
                        }
                    }
                    for acc in &mut vector {
                        *acc /= count.max(1.0);
                    }
                }
                vectors.push(normalize(vector));
            }
        } else {
            return Err(format!("unexpected onnx output shape {dims:?}"));
        }
        Ok(vectors)
    }

    /// Passages, batched; texts are sorted by length first so a batch pads to
    /// its own longest text, not the corpus's.
    pub(crate) fn embed_passages(
        &mut self,
        texts: &[String],
        batch: usize,
        progress: &mut dyn FnMut(usize) -> Result<(), String>,
    ) -> Result<Vec<Vec<f32>>, String> {
        let mut order: Vec<usize> = (0..texts.len()).collect();
        order.sort_by_key(|&i| texts[i].len());
        let mut out: Vec<Vec<f32>> = vec![Vec::new(); texts.len()];
        let mut done = 0;
        for chunk in order.chunks(batch.max(1)) {
            let prefixed: Vec<String> = chunk
                .iter()
                .map(|&i| format!("{}{}", self.spec.passage_prefix, texts[i]))
                .collect();
            for (&i, vector) in chunk.iter().zip(self.run(&prefixed)?) {
                out[i] = vector;
            }
            done += chunk.len();
            progress(done)?;
        }
        Ok(out)
    }

    pub(crate) fn embed_query(&mut self, text: &str) -> Result<Vec<f32>, String> {
        self.run(&[format!("{}{}", self.spec.query_prefix, text)])?
            .pop()
            .ok_or_else(|| "no query vector".to_string())
    }
}

fn normalize(mut vector: Vec<f32>) -> Vec<f32> {
    let norm = vector.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm > f32::EPSILON {
        for x in &mut vector {
            *x /= norm;
        }
    }
    vector
}

/// One loaded embedder per process, reused by queries and builds.
static EMBEDDER: OnceLock<Mutex<Option<EntityEmbedder>>> = OnceLock::new();

pub(crate) fn with_entity_embedder<T>(
    model_id: &str,
    threads: Option<usize>,
    work: impl FnOnce(&mut EntityEmbedder) -> Result<T, String>,
) -> Result<T, String> {
    // `threads: None` (a query) takes any loaded session of the model; a build
    // that asks for a thread count gets exactly that.
    let spec = entity_model_spec(model_id)?;
    let slot = EMBEDDER.get_or_init(|| Mutex::new(None));
    let mut guard = slot
        .lock()
        .map_err(|_| "entity embedder lock was poisoned".to_string())?;
    let reload = guard.as_ref().map_or(true, |loaded| {
        loaded.spec.model_id != spec.model_id || threads.is_some_and(|wanted| loaded.threads != wanted)
    });
    if reload {
        *guard = None;
        *guard = Some(EntityEmbedder::load(spec.model_id, embed_thread_budget(threads))?);
    }
    work(guard.as_mut().expect("embedder loaded"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn model_files_are_looked_for_where_fastembed_puts_them() {
        // the cow-tools unit sets both: fastembed downloads into HF_HOME
        assert_eq!(
            model_cache_root(Some("/srv/x/hf".into()), "/srv/x/fastembed".into()),
            PathBuf::from("/srv/x/hf")
        );
        assert_eq!(model_cache_root(None, ".fastembed_cache".into()), PathBuf::from(".fastembed_cache"));
        assert_eq!(model_cache_root(Some(" ".into()), "/c".into()), PathBuf::from("/c"));
    }

    #[test]
    fn e5_carries_its_own_prefixes_and_identity() {
        let spec = entity_model_spec("").unwrap();
        assert_eq!((spec.model_id, spec.dimensions), (DEFAULT_ENTITY_MODEL, 384));
        assert_eq!((spec.query_prefix, spec.passage_prefix), ("query: ", "passage: "));
        assert_eq!(entity_model_spec("e5-base").unwrap().dimensions, 768);
        assert!(entity_model_spec("bge-small").is_err());
        assert_eq!(embed_thread_budget(Some(0)), 1);
        assert_eq!(embed_thread_budget(Some(3)), 3);
    }
}

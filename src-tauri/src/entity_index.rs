//! The RDF entity index behind `find`: every entity of a graph's own RDF (and,
//! when asked, its documents) as one verbalized sentence, embedded with a
//! multilingual model, searched by meaning and by words together.
//!
//! - **Inert unless enabled**, per graph: `indexes/entities/config.json` says
//!   `"enabled": true` (`gardend index --graph <id>` writes it). Nothing else
//!   reads or writes this directory; hosted cells never enable it.
//! - **Files** (`graphs/<id>/indexes/entities/`): `manifest.json` (model,
//!   prefixes, verbalizer version, counts, provenance per named graph),
//!   `entities.jsonl` (one row per entity), `vectors.f32` (kept to reuse
//!   unchanged rows) and `vectors.i8` (one global scale; what queries scan).
//! - **Refresh** verbalizes every candidate (seconds), keeps the vector of any
//!   row whose content hash is unchanged, and embeds only the rest.
//! - **Search** is an exact scan of the i8 vectors fused with BM25 (0.7 / 0.3,
//!   each min-max normalized); a part (a distribution, a slice) answers as its
//!   parent. Measured in hub `plans/2026-10-14-agents-find-stuff/semantic-rdf.md`.

use crate::entity_embedder::{entity_model_spec, with_entity_embedder, DEFAULT_ENTITY_MODEL};
use crate::entity_verbalize::{Ctx, EntityRecord, VerbalizeCfg, VerbalizeStats};
use oxigraph::model::{GraphNameRef, NamedNodeRef, Term};
use oxigraph::store::Store;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, HashMap};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Instant;

pub(crate) const VERBALIZER_VERSION: &str = "rdf-entity-v2";
const ENTITY_INDEX_SCHEMA_VERSION: u32 = 1;
const FUSION_ALPHA: f32 = 0.7;
const CANDIDATES_PER_SIDE: usize = 50;

pub(crate) fn entity_index_dir(graph_dir: &Path) -> PathBuf {
    graph_dir.join("indexes").join("entities")
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct EntityIndexConfig {
    pub(crate) enabled: bool,
    #[serde(default)]
    pub(crate) model: String,
    /// Label and description preference, e.g. `["es", "en"]`.
    #[serde(default)]
    pub(crate) langs: Vec<String>,
    /// Also index the graph's documents (title + opening), as kind `document`.
    #[serde(default)]
    pub(crate) documents: bool,
}

impl EntityIndexConfig {
    pub(crate) fn model_id(&self) -> &str {
        if self.model.trim().is_empty() {
            DEFAULT_ENTITY_MODEL
        } else {
            self.model.trim()
        }
    }

    pub(crate) fn langs(&self) -> Vec<String> {
        if self.langs.is_empty() {
            vec!["en".into(), "es".into()]
        } else {
            self.langs.clone()
        }
    }
}

pub(crate) fn read_entity_index_config(graph_dir: &Path) -> Option<EntityIndexConfig> {
    let raw = std::fs::read(entity_index_dir(graph_dir).join("config.json")).ok()?;
    serde_json::from_slice(&raw).ok()
}

pub(crate) fn entity_index_enabled(graph_dir: &Path) -> bool {
    read_entity_index_config(graph_dir).is_some_and(|config| config.enabled)
}

pub(crate) fn write_entity_index_config(graph_dir: &Path, config: &EntityIndexConfig) -> Result<(), String> {
    entity_model_spec(config.model_id())?;
    let dir = entity_index_dir(graph_dir);
    std::fs::create_dir_all(&dir).map_err(|error| format!("create {}: {error}", dir.display()))?;
    write_atomic(
        &dir.join("config.json"),
        &serde_json::to_vec_pretty(config).map_err(|error| error.to_string())?,
    )
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct GraphProvenance {
    pub(crate) entities: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) rights: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) license: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) source: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct EntityIndexManifest {
    pub(crate) schema_version: u32,
    pub(crate) graph_id: String,
    pub(crate) model_id: String,
    pub(crate) dimensions: usize,
    pub(crate) query_prefix: String,
    pub(crate) passage_prefix: String,
    pub(crate) verbalizer: String,
    pub(crate) langs: Vec<String>,
    pub(crate) count: usize,
    pub(crate) vector_scale: f32,
    pub(crate) built_at: String,
    pub(crate) build_seconds: f64,
    pub(crate) verbalize_seconds: f64,
    pub(crate) embed_seconds: f64,
    pub(crate) embedded: usize,
    pub(crate) reused: usize,
    pub(crate) threads: usize,
    #[serde(default)]
    pub(crate) kinds: BTreeMap<String, usize>,
    #[serde(default)]
    pub(crate) graphs: BTreeMap<String, GraphProvenance>,
    #[serde(default)]
    pub(crate) stats: VerbalizeStats,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct EntityRow {
    pub(crate) iri: String,
    pub(crate) label: String,
    #[serde(rename = "type")]
    pub(crate) type_label: String,
    pub(crate) kind: String,
    pub(crate) graph: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) parent: Option<String>,
    pub(crate) text: String,
    pub(crate) hash: String,
    /// The entity's own `dct:license` (or `dct:rights`): a dataset's licence
    /// travels with it, unlike a whole graph's.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) license: Option<String>,
}

#[derive(Debug, Clone, Default)]
pub(crate) struct RefreshOptions {
    pub(crate) threads: Option<usize>,
    pub(crate) batch: Option<usize>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct EntityIndexReport {
    pub(crate) graph_id: String,
    pub(crate) model_id: String,
    pub(crate) entities: usize,
    pub(crate) embedded: usize,
    pub(crate) reused: usize,
    pub(crate) threads: usize,
    pub(crate) verbalize_seconds: f64,
    pub(crate) embed_seconds: f64,
    pub(crate) seconds: f64,
    pub(crate) kinds: BTreeMap<String, usize>,
    pub(crate) stats: VerbalizeStats,
}

impl EntityIndexReport {
    pub(crate) fn summary(&self) -> String {
        let rate = if self.embed_seconds > 0.0 { self.embedded as f64 / self.embed_seconds } else { 0.0 };
        format!(
            "entity index {}: {} entities ({} embedded, {} reused) with {} on {} thread(s); verbalize {:.1} s, embed {:.1} s ({:.1}/s), total {:.1} s",
            self.graph_id, self.entities, self.embedded, self.reused, self.model_id, self.threads,
            self.verbalize_seconds, self.embed_seconds, rate, self.seconds
        )
    }
}

fn content_hash(model_id: &str, passage_prefix: &str, text: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(VERBALIZER_VERSION.as_bytes());
    hasher.update([0]);
    hasher.update(model_id.as_bytes());
    hasher.update([0]);
    hasher.update(passage_prefix.as_bytes());
    hasher.update([0]);
    hasher.update(text.as_bytes());
    format!("{:x}", hasher.finalize())[..20].to_string()
}

/// Verbalize the whole graph: every candidate entity, in parallel over
/// `threads` workers, in a stable (sorted IRI) order.
pub(crate) fn verbalize_graph(
    store: &Store,
    graph_id: &str,
    langs: Vec<String>,
    threads: usize,
) -> (Vec<EntityRecord>, VerbalizeStats) {
    let cfg = VerbalizeCfg {
        langs,
        graph_id: graph_id.to_string(),
        ..VerbalizeCfg::default()
    };
    let ctx = Ctx::new(store, cfg);
    let candidates = ctx.candidates();
    let workers = threads.clamp(1, 8);
    let chunk = candidates.len().div_ceil(workers).max(1);
    let parts: Vec<(Vec<EntityRecord>, VerbalizeStats)> = std::thread::scope(|scope| {
        let handles: Vec<_> = candidates
            .chunks(chunk)
            .map(|slice| {
                let ctx = &ctx;
                scope.spawn(move || {
                    let mut stats = VerbalizeStats::default();
                    let records = slice.iter().filter_map(|iri| ctx.verbalize(iri, &mut stats)).collect();
                    (records, stats)
                })
            })
            .collect();
        handles.into_iter().map(|handle| handle.join().expect("verbalize worker")).collect()
    });
    let mut stats = VerbalizeStats { candidates: candidates.len(), ..VerbalizeStats::default() };
    let mut records = Vec::new();
    for (mut part, part_stats) in parts {
        records.append(&mut part);
        stats.kept += part_stats.kept;
        stats.skipped_denied_type += part_stats.skipped_denied_type;
        stats.skipped_no_label += part_stats.skipped_no_label;
        stats.skipped_excluded_graph += part_stats.skipped_excluded_graph;
        stats.truncated_scans += part_stats.truncated_scans;
        stats.label_fallback_local_name += part_stats.label_fallback_local_name;
    }
    (records, stats)
}

fn document_rows(graph_id: &str, graph_dir: &Path) -> Result<Vec<EntityRecord>, String> {
    let documents = crate::document_record_store::read_graph_documents_cold(graph_dir)?;
    Ok(documents
        .into_iter()
        .filter(|doc| !doc.title.trim().is_empty())
        .map(|doc| {
            let iri = if doc.rdf_subject.trim().is_empty() {
                crate::rdf::document_subject(&doc.document_id)
            } else {
                doc.rdf_subject.clone()
            };
            let body = crate::entity_verbalize::clean(&doc.body, 560);
            let text = if body.is_empty() {
                format!("{} — document", doc.title.trim())
            } else {
                format!("{} — document. {body}", doc.title.trim())
            };
            EntityRecord {
                iri,
                label: doc.title.trim().to_string(),
                label_lang: String::new(),
                types: vec!["document".to_string()],
                kind: "document".to_string(),
                graphs: vec![format!("{}:documents", crate::rdf::graph_subject(graph_id))],
                parent: None,
                text,
            }
        })
        .collect())
}

/// What a named graph says about itself: `dct:rights`, `dct:license` and
/// `dct:source` on the graph's own IRI, or on a `dcat:Catalog` / `void:Dataset`
/// it holds. Never a value some entity in it happens to carry (one dataset's
/// licence is not the catalogue's).
fn graph_provenance(store: &Store, graph_iri: &str) -> GraphProvenance {
    let mut provenance = GraphProvenance::default();
    let Ok(graph) = NamedNodeRef::new(graph_iri) else { return provenance };
    let graph_name = GraphNameRef::from(graph);
    let mut subjects: Vec<oxigraph::model::NamedNode> = vec![graph.into_owned()];
    let rdf_type = NamedNodeRef::new(crate::entity_verbalize::RDF_TYPE).expect("rdf:type");
    for class in ["http://www.w3.org/ns/dcat#Catalog", "http://rdfs.org/ns/void#Dataset"] {
        let class = NamedNodeRef::new(class).expect("class iri");
        for quad in store
            .quads_for_pattern(None, Some(rdf_type), Some(class.into()), Some(graph_name))
            .flatten()
            .take(8)
        {
            if let oxigraph::model::NamedOrBlankNode::NamedNode(node) = quad.subject {
                subjects.push(node);
            }
        }
    }
    let first = |predicate: &str| -> Option<String> {
        let predicate = NamedNodeRef::new(predicate).ok()?;
        subjects.iter().find_map(|subject| {
            store
                .quads_for_pattern(Some(subject.as_ref().into()), Some(predicate), None, Some(graph_name))
                .flatten()
                .find_map(|quad| object_text(quad.object))
        })
    };
    provenance.rights = first("http://purl.org/dc/terms/rights");
    provenance.license = first("http://purl.org/dc/terms/license");
    provenance.source = first("http://purl.org/dc/terms/source");
    provenance
}

fn object_text(object: Term) -> Option<String> {
    match object {
        Term::Literal(literal) => Some(crate::entity_verbalize::clean(literal.value(), 400)),
        Term::NamedNode(node) => Some(node.as_str().to_string()),
        _ => None,
    }
}

/// An entity's own licence: its `dct:license`, else its `dct:rights`.
pub(crate) fn entity_license(store: &Store, iri: &str) -> Option<String> {
    let subject = NamedNodeRef::new(iri).ok()?;
    ["http://purl.org/dc/terms/license", "http://purl.org/dc/terms/rights"]
        .iter()
        .find_map(|predicate| {
            let predicate = NamedNodeRef::new(predicate).ok()?;
            store
                .quads_for_pattern(Some(subject.into()), Some(predicate), None, None)
                .flatten()
                .find_map(|quad| object_text(quad.object))
        })
}

/// Build or refresh a graph's entity index. Refuses unless enabled.
pub(crate) fn refresh_entity_index(
    graph_id: &str,
    graph_dir: &Path,
    store: &Store,
    options: &RefreshOptions,
    progress: &mut dyn FnMut(&str, usize, usize) -> Result<(), String>,
) -> Result<EntityIndexReport, String> {
    let config = read_entity_index_config(graph_dir)
        .filter(|config| config.enabled)
        .ok_or_else(|| format!("the entity index of graph {graph_id} is not enabled (gardend index --graph {graph_id})"))?;
    let spec = entity_model_spec(config.model_id())?;
    let started = Instant::now();
    let threads = crate::entity_embedder::embed_thread_budget(options.threads);
    progress("verbalizing", 0, 0)?;
    let (mut records, stats) = verbalize_graph(store, graph_id, config.langs(), threads);
    if config.documents {
        records.extend(document_rows(graph_id, graph_dir)?);
    }
    let verbalize_seconds = started.elapsed().as_secs_f64();

    let rows: Vec<EntityRow> = records
        .into_iter()
        .map(|record| EntityRow {
            license: if record.kind == "document" { None } else { entity_license(store, &record.iri) },
            hash: content_hash(spec.model_id, spec.passage_prefix, &record.text),
            type_label: record.types.first().cloned().unwrap_or_else(|| record.kind.clone()),
            graph: record.graphs.first().cloned().unwrap_or_default(),
            iri: record.iri,
            label: record.label,
            kind: record.kind,
            parent: record.parent,
            text: record.text,
        })
        .collect();

    // reuse the vector of every unchanged row
    let previous = read_previous_vectors(graph_dir, spec.model_id, spec.dimensions);
    let mut vectors: Vec<Option<Vec<f32>>> = Vec::with_capacity(rows.len());
    let mut queue: Vec<usize> = Vec::new();
    for (i, row) in rows.iter().enumerate() {
        match previous.get(&(row.iri.clone(), row.hash.clone())) {
            Some(vector) => vectors.push(Some(vector.clone())),
            None => {
                vectors.push(None);
                queue.push(i);
            }
        }
    }
    drop(previous);
    let reused = rows.len() - queue.len();
    let total = queue.len();
    progress("embedding", 0, total)?;
    let embed_started = Instant::now();
    if !queue.is_empty() {
        let texts: Vec<String> = queue.iter().map(|&i| rows[i].text.clone()).collect();
        let embedded = with_entity_embedder(spec.model_id, Some(threads), |embedder| {
            embedder.embed_passages(&texts, options.batch.unwrap_or(32), &mut |done| progress("embedding", done, total))
        })?;
        for (&i, vector) in queue.iter().zip(embedded) {
            if vector.len() != spec.dimensions {
                return Err(format!("{} returned {} dimensions, expected {}", spec.model_id, vector.len(), spec.dimensions));
            }
            vectors[i] = Some(vector);
        }
    }
    let embed_seconds = embed_started.elapsed().as_secs_f64();
    let vectors: Vec<Vec<f32>> = vectors.into_iter().map(|v| v.expect("every row has a vector")).collect();

    progress("writing", rows.len(), rows.len())?;
    let mut kinds: BTreeMap<String, usize> = BTreeMap::new();
    let mut graphs: BTreeMap<String, GraphProvenance> = BTreeMap::new();
    for row in &rows {
        *kinds.entry(row.kind.clone()).or_default() += 1;
        graphs.entry(row.graph.clone()).or_default().entities += 1;
    }
    for (iri, provenance) in graphs.iter_mut() {
        let found = graph_provenance(store, iri);
        provenance.rights = found.rights;
        provenance.license = found.license;
        provenance.source = found.source;
    }
    let scale = write_index_files(graph_dir, &rows, &vectors, spec.dimensions)?;
    let manifest = EntityIndexManifest {
        schema_version: ENTITY_INDEX_SCHEMA_VERSION,
        graph_id: graph_id.to_string(),
        model_id: spec.model_id.to_string(),
        dimensions: spec.dimensions,
        query_prefix: spec.query_prefix.to_string(),
        passage_prefix: spec.passage_prefix.to_string(),
        verbalizer: VERBALIZER_VERSION.to_string(),
        langs: config.langs(),
        count: rows.len(),
        vector_scale: scale,
        built_at: crate::clock::timestamp(),
        build_seconds: started.elapsed().as_secs_f64(),
        verbalize_seconds,
        embed_seconds,
        embedded: total,
        reused,
        threads,
        kinds: kinds.clone(),
        graphs,
        stats: stats.clone(),
    };
    write_atomic(
        &entity_index_dir(graph_dir).join("manifest.json"),
        &serde_json::to_vec_pretty(&manifest).map_err(|error| error.to_string())?,
    )?;
    forget_loaded(graph_dir);
    Ok(EntityIndexReport {
        graph_id: graph_id.to_string(),
        model_id: spec.model_id.to_string(),
        entities: rows.len(),
        embedded: total,
        reused,
        threads,
        verbalize_seconds,
        embed_seconds,
        seconds: started.elapsed().as_secs_f64(),
        kinds,
        stats,
    })
}

fn write_atomic(path: &Path, bytes: &[u8]) -> Result<(), String> {
    let tmp = path.with_extension("tmp");
    {
        let mut file = std::fs::File::create(&tmp).map_err(|error| format!("write {}: {error}", tmp.display()))?;
        file.write_all(bytes).map_err(|error| format!("write {}: {error}", tmp.display()))?;
        file.sync_all().map_err(|error| format!("sync {}: {error}", tmp.display()))?;
    }
    std::fs::rename(&tmp, path).map_err(|error| format!("rename {}: {error}", path.display()))
}

fn write_index_files(graph_dir: &Path, rows: &[EntityRow], vectors: &[Vec<f32>], dims: usize) -> Result<f32, String> {
    let dir = entity_index_dir(graph_dir);
    std::fs::create_dir_all(&dir).map_err(|error| format!("create {}: {error}", dir.display()))?;
    let mut jsonl = Vec::with_capacity(rows.len() * 400);
    for row in rows {
        serde_json::to_writer(&mut jsonl, row).map_err(|error| error.to_string())?;
        jsonl.push(b'\n');
    }
    let max_abs = vectors
        .iter()
        .flat_map(|v| v.iter())
        .fold(0f32, |m, x| m.max(x.abs()))
        .max(1e-6);
    let scale = 127.0 / max_abs;
    let mut f32_bytes = Vec::with_capacity(vectors.len() * dims * 4);
    let mut i8_bytes = Vec::with_capacity(vectors.len() * dims);
    for vector in vectors {
        for x in vector {
            f32_bytes.extend_from_slice(&x.to_le_bytes());
            i8_bytes.push((x * scale).round().clamp(-127.0, 127.0) as i8 as u8);
        }
    }
    write_atomic(&dir.join("entities.jsonl"), &jsonl)?;
    write_atomic(&dir.join("vectors.f32"), &f32_bytes)?;
    write_atomic(&dir.join("vectors.i8"), &i8_bytes)?;
    Ok(scale)
}

pub(crate) fn read_entity_index_manifest(graph_dir: &Path) -> Option<EntityIndexManifest> {
    let raw = std::fs::read(entity_index_dir(graph_dir).join("manifest.json")).ok()?;
    serde_json::from_slice(&raw).ok()
}

fn read_rows(graph_dir: &Path) -> Result<Vec<EntityRow>, String> {
    let raw = std::fs::read(entity_index_dir(graph_dir).join("entities.jsonl"))
        .map_err(|error| format!("read entities.jsonl: {error}"))?;
    raw.split(|b| *b == b'\n')
        .filter(|line| !line.is_empty())
        .map(|line| serde_json::from_slice(line).map_err(|error| format!("entities.jsonl: {error}")))
        .collect()
}

fn read_previous_vectors(graph_dir: &Path, model_id: &str, dims: usize) -> HashMap<(String, String), Vec<f32>> {
    let mut out = HashMap::new();
    let Some(manifest) = read_entity_index_manifest(graph_dir) else { return out };
    if manifest.model_id != model_id || manifest.dimensions != dims {
        return out;
    }
    let (Ok(rows), Ok(bytes)) = (read_rows(graph_dir), std::fs::read(entity_index_dir(graph_dir).join("vectors.f32"))) else {
        return out;
    };
    if bytes.len() != rows.len() * dims * 4 {
        return out;
    }
    for (i, row) in rows.into_iter().enumerate() {
        let vector = bytes[i * dims * 4..(i + 1) * dims * 4]
            .chunks_exact(4)
            .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
            .collect();
        out.insert((row.iri, row.hash), vector);
    }
    out
}

// ---------------------------------------------------------------- loaded indexes

pub(crate) struct LoadedEntityIndex {
    pub(crate) manifest: EntityIndexManifest,
    pub(crate) rows: Vec<EntityRow>,
    codes: Vec<i8>,
    by_iri: HashMap<String, usize>,
    bm25: Bm25,
}

static LOADED: OnceLock<Mutex<HashMap<PathBuf, Arc<LoadedEntityIndex>>>> = OnceLock::new();

fn forget_loaded(graph_dir: &Path) {
    if let Ok(mut loaded) = LOADED.get_or_init(|| Mutex::new(HashMap::new())).lock() {
        loaded.remove(graph_dir);
    }
}

/// The index of a graph, loaded once per build (re-read when its manifest changes).
pub(crate) fn load_entity_index(graph_dir: &Path) -> Result<Arc<LoadedEntityIndex>, String> {
    let manifest = read_entity_index_manifest(graph_dir).ok_or_else(|| "the entity index has not been built yet".to_string())?;
    let cache = LOADED.get_or_init(|| Mutex::new(HashMap::new()));
    if let Some(index) = cache.lock().map_err(|_| "entity index cache poisoned")?.get(graph_dir) {
        if index.manifest.built_at == manifest.built_at && index.manifest.count == manifest.count {
            return Ok(index.clone());
        }
    }
    let rows = read_rows(graph_dir)?;
    let codes: Vec<i8> = std::fs::read(entity_index_dir(graph_dir).join("vectors.i8"))
        .map_err(|error| format!("read vectors.i8: {error}"))?
        .into_iter()
        .map(|b| b as i8)
        .collect();
    if codes.len() != rows.len() * manifest.dimensions {
        return Err(format!(
            "entity index is inconsistent: {} rows, {} vector bytes at {} dimensions; rebuild it",
            rows.len(),
            codes.len(),
            manifest.dimensions
        ));
    }
    let by_iri = rows.iter().enumerate().map(|(i, row)| (row.iri.clone(), i)).collect();
    let bm25 = Bm25::build(rows.iter().map(|row| (row.label.as_str(), row.text.as_str())));
    let index = Arc::new(LoadedEntityIndex { manifest, rows, codes, by_iri, bm25 });
    cache
        .lock()
        .map_err(|_| "entity index cache poisoned")?
        .insert(graph_dir.to_path_buf(), index.clone());
    Ok(index)
}

// ---------------------------------------------------------------- ranking

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FindMode {
    Fused,
    Vector,
    Lexical,
}

#[derive(Debug, Clone)]
pub(crate) struct FindFilter {
    pub(crate) kinds: Vec<String>,
    pub(crate) types: Vec<String>,
    pub(crate) graphs: Vec<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct RankedHit {
    /// The row shown (a part's parent when there is one).
    pub(crate) row: usize,
    /// The part that matched, when it rolled up into `row`.
    pub(crate) via: Option<usize>,
    pub(crate) score: f32,
}

impl LoadedEntityIndex {
    fn allowed(&self, i: usize, filter: &FindFilter) -> bool {
        let row = &self.rows[i];
        (filter.kinds.is_empty() || filter.kinds.iter().any(|k| k.eq_ignore_ascii_case(&row.kind)))
            && (filter.types.is_empty() || filter.types.iter().any(|t| t.eq_ignore_ascii_case(&row.type_label)))
            && (filter.graphs.is_empty() || filter.graphs.iter().any(|g| *g == row.graph))
    }

    fn vector_candidates(&self, query: &[f32], filter: &FindFilter, k: usize) -> Vec<(u32, f32)> {
        let dims = self.manifest.dimensions;
        let scale = self.manifest.vector_scale;
        let q: Vec<i8> = query.iter().map(|x| (x * scale).round().clamp(-127.0, 127.0) as i8).collect();
        let inv = 1.0 / (scale * scale);
        let mut top = TopK::new(k);
        for (i, row) in self.codes.chunks_exact(dims).enumerate() {
            if !self.allowed(i, filter) {
                continue;
            }
            top.push(i as u32, dot_i8(&q, row) as f32 * inv);
        }
        top.into_vec()
    }

    /// Rank rows for a query. `query_vector` is required unless `mode` is lexical.
    pub(crate) fn rank(
        &self,
        query_vector: Option<&[f32]>,
        query: &str,
        filter: &FindFilter,
        mode: FindMode,
        k: usize,
    ) -> Vec<RankedHit> {
        let vector = match (mode, query_vector) {
            (FindMode::Lexical, _) | (_, None) => Vec::new(),
            (_, Some(q)) => self.vector_candidates(q, filter, CANDIDATES_PER_SIDE),
        };
        let lexical = if mode == FindMode::Vector {
            Vec::new()
        } else {
            self.bm25
                .search(query)
                .into_iter()
                .filter(|(i, _)| self.allowed(*i as usize, filter))
                .take(CANDIDATES_PER_SIDE)
                .collect()
        };
        let fused = match mode {
            FindMode::Vector => vector,
            FindMode::Lexical => normalize_scores(&lexical),
            FindMode::Fused => fuse(&vector, &lexical, FUSION_ALPHA),
        };
        let mut hits: Vec<RankedHit> = Vec::new();
        for (i, score) in fused {
            let i = i as usize;
            let asked_for_parts = filter.kinds.iter().any(|k| k.eq_ignore_ascii_case(&self.rows[i].kind));
            let parent = self.rows[i].parent.as_ref().and_then(|p| self.by_iri.get(p)).filter(|_| !asked_for_parts);
            let (row, via) = match parent {
                Some(&parent) => (parent, Some(i)),
                None => (i, None),
            };
            if hits.iter().any(|hit| hit.row == row) {
                continue;
            }
            hits.push(RankedHit { row, via, score });
            if hits.len() >= k {
                break;
            }
        }
        hits
    }

    /// One hit, as an agent reads it.
    pub(crate) fn hit_json(&self, hit: &RankedHit, default_graph: &str) -> Value {
        let row = &self.rows[hit.row];
        let says = {
            let rest = row.text.split_once(". ").map_or(row.text.as_str(), |(_, rest)| rest);
            let cut: String = rest.chars().take(140).collect();
            if rest.chars().count() > 140 { format!("{cut}…") } else { cut }
        };
        let mut out = json!({
            "iri": row.iri,
            "label": row.label,
            "type": row.type_label,
            "score": ((hit.score as f64) * 100.0).round() / 100.0,
            "says": says,
        });
        if let Some(via) = hit.via {
            out["via"] = json!({ "label": self.rows[via].label, "iri": self.rows[via].iri });
        }
        if let Some(license) = &row.license {
            out["license"] = json!(license);
        }
        if row.graph != default_graph {
            out["graph"] = json!(row.graph);
        }
        out
    }
}

fn normalize_scores(list: &[(u32, f32)]) -> Vec<(u32, f32)> {
    let max = list.iter().map(|x| x.1).fold(f32::MIN, f32::max);
    let min = list.iter().map(|x| x.1).fold(f32::MAX, f32::min);
    let span = (max - min).max(1e-6);
    list.iter().map(|&(i, s)| (i, (s - min) / span)).collect()
}

/// Convex combination of min-max normalized scores (absent = 0).
pub(crate) fn fuse(vector: &[(u32, f32)], lexical: &[(u32, f32)], alpha: f32) -> Vec<(u32, f32)> {
    let mut scores: HashMap<u32, f32> = HashMap::new();
    for (i, s) in normalize_scores(vector) {
        *scores.entry(i).or_default() += alpha * s;
    }
    for (i, s) in normalize_scores(lexical) {
        *scores.entry(i).or_default() += (1.0 - alpha) * s;
    }
    let mut out: Vec<(u32, f32)> = scores.into_iter().collect();
    out.sort_by(|a, b| b.1.total_cmp(&a.1).then(a.0.cmp(&b.0)));
    out
}

struct TopK {
    k: usize,
    items: Vec<(u32, f32)>,
}

impl TopK {
    fn new(k: usize) -> Self {
        Self { k: k.max(1), items: Vec::with_capacity(k + 1) }
    }
    fn push(&mut self, id: u32, score: f32) {
        if self.items.len() == self.k {
            if score <= self.items[self.k - 1].1 {
                return;
            }
            self.items.pop();
        }
        let at = self.items.partition_point(|x| x.1 >= score);
        self.items.insert(at, (id, score));
    }
    fn into_vec(self) -> Vec<(u32, f32)> {
        self.items
    }
}

fn dot_i8(a: &[i8], b: &[i8]) -> i32 {
    #[cfg(target_arch = "x86_64")]
    {
        if std::is_x86_feature_detected!("avx2") {
            // SAFETY: avx2 was detected at runtime just above.
            return unsafe { dot_i8_avx2(a, b) };
        }
    }
    a.iter().zip(b).map(|(&x, &y)| x as i32 * y as i32).sum()
}

#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2")]
unsafe fn dot_i8_avx2(a: &[i8], b: &[i8]) -> i32 {
    use std::arch::x86_64::*;
    let n = a.len().min(b.len());
    let mut acc = _mm256_setzero_si256();
    let mut i = 0;
    while i + 16 <= n {
        let va = _mm256_cvtepi8_epi16(_mm_loadu_si128(a.as_ptr().add(i) as *const __m128i));
        let vb = _mm256_cvtepi8_epi16(_mm_loadu_si128(b.as_ptr().add(i) as *const __m128i));
        acc = _mm256_add_epi32(acc, _mm256_madd_epi16(va, vb));
        i += 16;
    }
    let mut lanes = [0i32; 8];
    _mm256_storeu_si256(lanes.as_mut_ptr() as *mut __m256i, acc);
    let mut sum: i32 = lanes.iter().sum();
    while i < n {
        sum += a[i] as i32 * b[i] as i32;
        i += 1;
    }
    sum
}

// ---------------------------------------------------------------- BM25

const STOPWORDS: &[&str] = &[
    "a", "al", "an", "and", "are", "as", "at", "be", "by", "con", "de", "del", "el", "en", "es", "for", "from", "how",
    "in", "is", "it", "la", "las", "lo", "los", "of", "on", "or", "para", "por", "que", "se", "su", "sus", "the", "to",
    "un", "una", "what", "where", "which", "who", "with", "y", "o", "u", "about", "find", "sobre", "busco", "quiero",
    "cual", "donde", "como", "hay", "there", "any", "some", "me",
];

fn fold(c: char) -> char {
    match c {
        'á' | 'à' | 'ä' | 'â' | 'ã' | 'Á' | 'À' | 'Ä' | 'Â' | 'Ã' => 'a',
        'é' | 'è' | 'ë' | 'ê' | 'É' | 'È' | 'Ë' | 'Ê' => 'e',
        'í' | 'ì' | 'ï' | 'î' | 'Í' | 'Ì' | 'Ï' | 'Î' => 'i',
        'ó' | 'ò' | 'ö' | 'ô' | 'õ' | 'Ó' | 'Ò' | 'Ö' | 'Ô' | 'Õ' => 'o',
        'ú' | 'ù' | 'ü' | 'û' | 'Ú' | 'Ù' | 'Ü' | 'Û' => 'u',
        'ñ' | 'Ñ' => 'n',
        'ç' | 'Ç' => 'c',
        c => c.to_lowercase().next().unwrap_or(c),
    }
}

/// Accent-folded, lower-cased tokens of two or more characters, stopwords out.
pub(crate) fn lexical_tokens(text: &str) -> Vec<String> {
    let folded: String = text.chars().map(fold).collect();
    folded
        .split(|c: char| !c.is_alphanumeric())
        .filter(|token| token.len() >= 2 && !STOPWORDS.contains(token))
        .map(str::to_string)
        .collect()
}

struct Bm25 {
    postings: HashMap<String, Vec<(u32, u16)>>,
    lengths: Vec<u32>,
    average: f32,
}

impl Bm25 {
    /// The label counts twice, so a name match outranks a passing mention.
    fn build<'a>(docs: impl Iterator<Item = (&'a str, &'a str)>) -> Self {
        let mut postings: HashMap<String, Vec<(u32, u16)>> = HashMap::new();
        let mut lengths = Vec::new();
        for (i, (label, text)) in docs.enumerate() {
            let mut tf: HashMap<String, u16> = HashMap::new();
            let mut tokens = lexical_tokens(label);
            tokens.extend(lexical_tokens(label));
            tokens.extend(lexical_tokens(text));
            lengths.push(tokens.len() as u32);
            for token in tokens {
                *tf.entry(token).or_default() += 1;
            }
            for (token, n) in tf {
                postings.entry(token).or_default().push((i as u32, n));
            }
        }
        let average = lengths.iter().map(|&n| n as f32).sum::<f32>() / lengths.len().max(1) as f32;
        Self { postings, lengths, average }
    }

    fn search(&self, query: &str) -> Vec<(u32, f32)> {
        let n = self.lengths.len() as f32;
        let mut scores: HashMap<u32, f32> = HashMap::new();
        let mut tokens = lexical_tokens(query);
        tokens.sort();
        tokens.dedup();
        for token in tokens {
            let Some(postings) = self.postings.get(&token) else { continue };
            let df = postings.len() as f32;
            let idf = ((n - df + 0.5) / (df + 0.5) + 1.0).ln();
            for &(doc, tf) in postings {
                let tf = tf as f32;
                let norm = 0.25 + 0.75 * self.lengths[doc as usize] as f32 / self.average.max(1.0);
                *scores.entry(doc).or_default() += idf * tf * 2.2 / (tf + 1.2 * norm);
            }
        }
        let mut out: Vec<(u32, f32)> = scores.into_iter().collect();
        out.sort_by(|a, b| b.1.total_cmp(&a.1).then(a.0.cmp(&b.0)));
        out
    }
}

#[cfg(test)]
#[path = "entity_index_tests.rs"]
mod tests;

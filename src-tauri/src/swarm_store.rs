//! Immutable analysis snapshots, independent of the CRDT write/projection queue.
//! Build and validate outside the publication lock; existing readers keep their Arc.
use oxigraph::{
    io::{RdfFormat, RdfParser},
    model::{GraphName, NamedNode, NamedOrBlankNode},
    store::Store,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, HashMap, HashSet},
    fs::{self, File},
    io::{BufReader, Read},
    path::{Component, Path, PathBuf},
    sync::{Arc, Mutex, OnceLock, RwLock},
};

type Result<T> = std::result::Result<T, String>;

#[derive(Clone, Deserialize, Serialize)]
pub(crate) struct SourceSpec {
    pub(crate) rdf_path: PathBuf,
    pub(crate) embeddings_manifest_path: PathBuf,
    pub(crate) manifest_sha256: String,
    pub(crate) generation: String,
}

#[derive(Clone, Deserialize, Serialize)]
pub(crate) struct EmbeddingMeta {
    pub(crate) id: String,
    pub(crate) graph: String,
    pub(crate) kind: String,
    #[serde(default)]
    pub(crate) text: String,
}

pub(crate) struct Embedding {
    pub(crate) meta: EmbeddingMeta,
    pub(crate) vector: Vec<f32>,
}

#[derive(Deserialize)]
struct Manifest {
    schema_version: u32,
    rdf: PathBuf,
    rdf_sha256: String,
    vectors: Vec<VectorFile>,
}

#[derive(Deserialize)]
struct VectorFile {
    file: PathBuf,
    dimensions: usize,
    rows: Vec<EmbeddingMeta>,
    sha256: String,
}

#[derive(Clone)]
pub(crate) struct Snapshot {
    pub(crate) store: Store,
    pub(crate) embeddings: Arc<Vec<Embedding>>,
    pub(crate) graphs: Vec<NamedNode>,
    pub(crate) generation: String,
    pub(crate) dimensions: usize,
}

pub(crate) struct SwarmCell {
    snapshot: RwLock<Option<Arc<Snapshot>>>,
    pub(crate) queries: RwLock<Arc<BTreeMap<String, String>>>,
    // Import/save serialization is separate from publication. Reads never take it.
    pub(crate) writer: Mutex<()>,
    pub(crate) directory: PathBuf,
}

static CELLS: OnceLock<Mutex<HashMap<PathBuf, Arc<SwarmCell>>>> = OnceLock::new();

pub(crate) fn cell(directory: &Path, incarnation: &str) -> Result<Arc<SwarmCell>> {
    let directory = directory
        .canonicalize()
        .map_err(|e| format!("swarm graph directory: {e}"))?;
    // Key both memory and metadata by the graph incarnation; recreating a graph
    // at the same path cannot inherit a retired snapshot or saved query map.
    let directory = directory
        .join("swarm")
        .join(format!("{:x}", Sha256::digest(incarnation.as_bytes())));
    let mut cells = CELLS
        .get_or_init(Default::default)
        .lock()
        .map_err(|_| "swarm cell map poisoned")?;
    if let Some(cell) = cells.get(&directory) {
        return Ok(Arc::clone(cell));
    }
    let query_path = directory.join("queries.json");
    let queries = if query_path.exists() {
        read_json(&query_path)?
    } else {
        BTreeMap::new()
    };
    let entry = Arc::new(SwarmCell {
        snapshot: RwLock::new(None),
        queries: RwLock::new(Arc::new(queries)),
        writer: Mutex::new(()),
        directory: directory.clone(),
    });
    cells.insert(directory, Arc::clone(&entry));
    Ok(entry)
}

impl SwarmCell {
    pub(crate) fn snapshot(&self) -> Result<Arc<Snapshot>> {
        if let Some(snapshot) = self
            .snapshot
            .read()
            .map_err(|_| "swarm snapshot poisoned")?
            .as_ref()
        {
            return Ok(Arc::clone(snapshot));
        }
        // Only first access after restart waits for initial package reconstruction.
        // A loaded snapshot's read path returns above without this writer lock.
        let _writer = self
            .writer
            .lock()
            .map_err(|_| "swarm import lock poisoned")?;
        if let Some(snapshot) = self
            .snapshot
            .read()
            .map_err(|_| "swarm snapshot poisoned")?
            .as_ref()
        {
            return Ok(Arc::clone(snapshot));
        }
        let spec: SourceSpec = read_json(&self.directory.join("source.json"))
            .map_err(|e| format!("swarm snapshot not loaded; run swarm_load: {e}"))?;
        let snapshot = Arc::new(Snapshot::load(&spec)?);
        self.publish(Arc::clone(&snapshot))?;
        Ok(snapshot)
    }

    pub(crate) fn publish(&self, snapshot: Arc<Snapshot>) -> Result<()> {
        let mut guard = self
            .snapshot
            .write()
            .map_err(|_| "swarm snapshot poisoned")?;
        let previous = guard.replace(snapshot);
        drop(guard);
        drop(previous); // Store teardown must not run under the publication lock.
        Ok(())
    }

    pub(crate) fn import(&self, rdf: &Path, manifest: &Path) -> Result<Arc<Snapshot>> {
        let _writer = self
            .writer
            .lock()
            .map_err(|_| "swarm import lock poisoned")?;
        let manifest = manifest
            .canonicalize()
            .map_err(|e| format!("embedding manifest path: {e}"))?;
        let spec = SourceSpec {
            rdf_path: rdf
                .canonicalize()
                .map_err(|e| format!("RDF package path: {e}"))?,
            manifest_sha256: sha256_file(&manifest)?,
            embeddings_manifest_path: manifest,
            generation: uuid::Uuid::new_v4().to_string(),
        };
        let snapshot = Arc::new(Snapshot::load(&spec)?);
        // A failed import leaves both the old source pointer and old snapshot intact.
        write_json(&self.directory.join("source.json"), &spec)?;
        self.publish(Arc::clone(&snapshot))?;
        Ok(snapshot)
    }
}

pub(crate) fn read_json<T: serde::de::DeserializeOwned>(path: &Path) -> Result<T> {
    serde_json::from_reader(BufReader::new(
        File::open(path).map_err(|e| format!("{}: {e}", path.display()))?,
    ))
    .map_err(|e| format!("{}: {e}", path.display()))
}

pub(crate) fn sha256_file(path: &Path) -> Result<String> {
    let mut reader =
        BufReader::new(File::open(path).map_err(|e| format!("{}: {e}", path.display()))?);
    let mut hasher = Sha256::new();
    let mut buffer = [0_u8; 65_536];
    loop {
        let n = reader.read(&mut buffer).map_err(|e| e.to_string())?;
        if n == 0 {
            break;
        }
        hasher.update(&buffer[..n]);
    }
    Ok(format!("{:x}", hasher.finalize()))
}

pub(crate) fn write_json(path: &Path, value: &impl Serialize) -> Result<()> {
    fs::create_dir_all(path.parent().ok_or("swarm metadata has no parent")?)
        .map_err(|e| e.to_string())?;
    let temporary = path.with_extension(format!("{}.tmp", uuid::Uuid::new_v4()));
    let file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temporary)
        .map_err(|e| e.to_string())?;
    serde_json::to_writer(file, value).map_err(|e| e.to_string())?;
    // New, rebuildable analysis metadata only; no authority/durability machinery is changed.
    fs::rename(&temporary, path).map_err(|e| e.to_string())
}

pub(crate) fn normalize(vector: &mut [f32]) -> Result<()> {
    if vector.is_empty() || vector.iter().any(|v| !v.is_finite()) {
        return Err("vector must be nonempty and finite".into());
    }
    let norm = vector
        .iter()
        .map(|v| f64::from(*v).powi(2))
        .sum::<f64>()
        .sqrt();
    if norm == 0.0 || !norm.is_finite() {
        return Err("vector norm must be positive and finite".into());
    }
    for value in vector {
        *value = (f64::from(*value) / norm) as f32;
    }
    Ok(())
}

impl Snapshot {
    pub(crate) fn load(spec: &SourceSpec) -> Result<Self> {
        if sha256_file(&spec.embeddings_manifest_path)? != spec.manifest_sha256 {
            return Err("swarm source manifest changed since publication".into());
        }
        let manifest: Manifest = read_json(&spec.embeddings_manifest_path)?;
        if manifest.schema_version != 1 {
            return Err("unsupported embedding schema_version".into());
        }
        let package = spec
            .embeddings_manifest_path
            .parent()
            .ok_or("manifest has no parent")?
            .canonicalize()
            .map_err(|e| e.to_string())?;
        let rdf = package_file(&package, &manifest.rdf)?;
        if rdf != spec.rdf_path.canonicalize().map_err(|e| e.to_string())? {
            return Err("rdf_path differs from the manifest RDF file".into());
        }
        if sha256_file(&rdf)? != manifest.rdf_sha256 {
            return Err("swarm RDF sha256 differs from manifest".into());
        }
        let store = Store::new().map_err(|e| e.to_string())?;
        // Oxigraph 0.5: bulk-loaded quads are invisible until commit(); without it every check below passes on an empty store.
        let mut loader = store.bulk_loader();
        loader
            .load_from_reader(
                RdfParser::from_format(RdfFormat::NQuads),
                BufReader::new(File::open(&spec.rdf_path).map_err(|e| e.to_string())?),
            )
            .map_err(|e| format!("load swarm N-Quads: {e}"))?;
        loader
            .commit()
            .map_err(|e| format!("commit swarm N-Quads: {e}"))?;
        let mut graphs = Vec::new();
        // All analysis evidence must have explicit provenance; default/blank graphs
        // would evade the request's named-graph exclusion contract.
        for quad in store.iter() {
            if !matches!(
                quad.map_err(|e| e.to_string())?.graph_name,
                GraphName::NamedNode(_)
            ) {
                return Err("swarm RDF requires a named IRI graph for every quad".into());
            }
        }
        for graph in store.named_graphs() {
            match graph.map_err(|e| e.to_string())? {
                NamedOrBlankNode::NamedNode(node) => graphs.push(node),
                _ => return Err("swarm RDF does not accept blank graph names".into()),
            }
        }
        graphs.sort();
        let mut embeddings = Vec::new();
        let mut dimensions = 0;
        let mut seen = HashSet::new();
        for group in manifest.vectors {
            if group.dimensions == 0 || group.dimensions > 65_536 {
                return Err("invalid embedding dimensions".into());
            }
            if dimensions != 0 && dimensions != group.dimensions {
                return Err("embedding groups have different dimensions".into());
            }
            dimensions = group.dimensions;
            let path = package_file(&package, &group.file)?;
            if sha256_file(&path)? != group.sha256 {
                return Err("swarm embedding sha256 differs from manifest".into());
            }
            let bytes = group
                .rows
                .len()
                .checked_mul(dimensions)
                .and_then(|n| n.checked_mul(4))
                .ok_or("embedding byte length overflow")?;
            let file = File::open(path).map_err(|e| e.to_string())?;
            if file.metadata().map_err(|e| e.to_string())?.len() != bytes as u64 {
                return Err("embedding byte length does not match manifest rows/dimensions".into());
            }
            let mut reader = BufReader::new(file);
            for meta in group.rows {
                let id = NamedNode::new(meta.id.clone()).map_err(|e| e.to_string())?;
                let graph = NamedNode::new(meta.graph.clone()).map_err(|e| e.to_string())?;
                if !seen.insert((meta.id.clone(), meta.kind.clone())) {
                    return Err("duplicate embedding id/kind".into());
                }
                match store
                    .quads_for_pattern(
                        Some(id.as_ref().into()),
                        None,
                        None,
                        Some(graph.as_ref().into()),
                    )
                    .next()
                {
                    Some(Ok(_)) => {}
                    Some(Err(e)) => return Err(e.to_string()),
                    None => {
                        return Err(format!(
                            "embedding subject/graph absent from RDF: {} {}",
                            meta.id, meta.graph
                        ))
                    }
                }
                let mut row = vec![0_u8; dimensions * 4];
                reader.read_exact(&mut row).map_err(|e| e.to_string())?;
                let mut vector: Vec<f32> = row
                    .chunks_exact(4)
                    .map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]]))
                    .collect();
                normalize(&mut vector)?;
                embeddings.push(Embedding { meta, vector });
            }
        }
        if embeddings.is_empty() {
            return Err("swarm import requires real embedding rows".into());
        }
        embeddings.sort_by(|a, b| {
            a.meta
                .id
                .cmp(&b.meta.id)
                .then(a.meta.kind.cmp(&b.meta.kind))
        });
        Ok(Self {
            store,
            embeddings: Arc::new(embeddings),
            graphs,
            generation: spec.generation.clone(),
            dimensions,
        })
    }
}

fn package_file(package: &Path, file: &Path) -> Result<PathBuf> {
    if file.is_absolute()
        || file
            .components()
            .any(|c| !matches!(c, Component::Normal(_)))
    {
        return Err("swarm source file must be a relative package path without traversal".into());
    }
    let path = package
        .join(file)
        .canonicalize()
        .map_err(|e| e.to_string())?;
    if !path.starts_with(package) {
        return Err("swarm source file escaped its package".into());
    }
    Ok(path)
}

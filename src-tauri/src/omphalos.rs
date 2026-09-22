use crate::{
    app_runtime::AppHandle,
    emporium::{
        contract::nomos_vocabulary,
        reconcile::{reconcile_classes_validated, ClassScope, Placement, SpanKey},
        survey::parse_term,
        terms::{Term, Triple, TripleDiff},
    },
    paths::profile_dir,
    runtime_config::RDF_TYPE,
    storage::{create_dir_all, display_path},
};
use oxigraph::{
    io::{RdfFormat, RdfParser},
    sparql::{QueryResults, SparqlEvaluator},
    store::Store,
};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
    sync::{Arc, Mutex, OnceLock},
};

pub(crate) const NOMOS_NS: &str = "http://mnemosyne.dev/nomos#";
pub(crate) const WORLD_SUBJECT: &str = "urn:mnemosyne:local:world";
const CONSTITUTION_FILE: &str = "constitution.ttl";

#[cfg_attr(not(test), allow(dead_code))]
pub(crate) const DEFAULT_CONSTITUTION_TTL: &str = r#"@prefix nomos: <http://mnemosyne.dev/nomos#> .
@prefix xsd:   <http://www.w3.org/2001/XMLSchema#> .

<urn:mnemosyne:local:world>
  a nomos:World ;
  nomos:schemaVersion 2 ;
  nomos:identityAnchor "rdf-iri" ;
  nomos:usesEmbedder <urn:mnemosyne:local:embedder:bge-small-en-v1.5-q> ;
  nomos:hostsGraph    <urn:mnemosyne:local:graph:default> ;
  nomos:usesOracle    <urn:mnemosyne:local:oracle:kg-ultra> ;
  nomos:hasOracleBinding <urn:mnemosyne:local:oracle-binding:kg-ultra:default> .

<urn:mnemosyne:local:embedder:bge-small-en-v1.5-q>
  a nomos:Embedder ;
  nomos:providerId "fastembed" ;
  nomos:modelId    "fastembed/qdrant/bge-small-en-v1.5-onnx-q" ;
  nomos:dimensions 384 .

<urn:mnemosyne:local:graph:default>
  a nomos:Graph ;
  nomos:graphId "default" .

<urn:mnemosyne:local:oracle:kg-ultra>
  a nomos:Oracle ;
  nomos:oracleKind "kg-ultra" ;
  nomos:serviceId "kg-ultra" ;
  nomos:enabled true ;
  nomos:inputLayer "rdf-graph" ;
  nomos:outputVocab "kg-ultra-intuition" ;
  nomos:outputVocabVersion "1.1.0" ;
  nomos:projectionTarget "projection:kg-ultra" ;
  nomos:supportsTask <urn:mnemosyne:local:oracle-task:kg-ultra:link-prediction>,
                     <urn:mnemosyne:local:oracle-task:kg-ultra:logical-query-answering> .

<urn:mnemosyne:local:oracle-task:kg-ultra:link-prediction>
  a nomos:OracleTask ;
  nomos:taskId "link-prediction" ;
  nomos:taskKind "link-prediction" ;
  nomos:modelId "ultra_4g" ;
  nomos:modelVersion "zero-shot" ;
  nomos:supportsPath false ;
  nomos:supportsIntersection false ;
  nomos:supportsUnion false ;
  nomos:supportsNegation false ;
  nomos:defaultTopK 50 ;
  nomos:defaultMinScore "0.20"^^xsd:double .

<urn:mnemosyne:local:oracle-task:kg-ultra:logical-query-answering>
  a nomos:OracleTask ;
  nomos:taskId "logical-query-answering" ;
  nomos:taskKind "logical-query-answering" ;
  nomos:modelId "ultraquery_4g" ;
  nomos:modelVersion "zero-shot" ;
  nomos:supportsPath true ;
  nomos:supportsIntersection true ;
  nomos:supportsUnion true ;
  nomos:supportsNegation false ;
  nomos:defaultTopK 50 ;
  nomos:defaultMinScore "0.20"^^xsd:double .

<urn:mnemosyne:local:oracle-binding:kg-ultra:default>
  a nomos:OracleBinding ;
  nomos:usesOracle <urn:mnemosyne:local:oracle:kg-ultra> ;
  nomos:graphId "*" ;
  nomos:mode "active" ;
  nomos:allowedTask "link-prediction", "logical-query-answering" ;
  nomos:topK 50 ;
  nomos:minScore "0.20"^^xsd:double ;
  nomos:maxHops 3 ;
  nomos:allowPath true ;
  nomos:allowIntersection true ;
  nomos:allowUnion true ;
  nomos:allowNegation false ;
  nomos:structuralWeight "0.55"^^xsd:double ;
  nomos:embeddingWeight "0.35"^^xsd:double ;
  nomos:lexicalWeight "0.10"^^xsd:double ;
  nomos:persistPolicy "on-request" ;
  nomos:requiresAcceptance true ;
  nomos:maxPersistedCandidates 25 .
"#;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Constitution {
    pub(crate) schema_version: u32,
    pub(crate) identity_anchor: String,
    pub(crate) embedder: EmbedderSelection,
    pub(crate) graphs: Vec<GraphEntry>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct EmbedderSelection {
    pub(crate) provider_id: String,
    pub(crate) model_id: String,
    pub(crate) dimensions: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct GraphEntry {
    pub(crate) graph_id: String,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct OracleConstitution {
    pub(crate) oracle: OracleSelection,
    pub(crate) tasks: Vec<OracleTaskSelection>,
    pub(crate) binding: OracleBinding,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct OracleSelection {
    pub(crate) subject: String,
    pub(crate) oracle_kind: String,
    pub(crate) service_id: String,
    pub(crate) enabled: bool,
    pub(crate) input_layer: String,
    pub(crate) output_vocab: String,
    pub(crate) output_vocab_version: Option<String>,
    pub(crate) projection_target: String,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct OracleTaskSelection {
    pub(crate) subject: String,
    pub(crate) task_id: String,
    pub(crate) task_kind: String,
    pub(crate) model_id: String,
    pub(crate) model_version: Option<String>,
    pub(crate) supports_path: bool,
    pub(crate) supports_intersection: bool,
    pub(crate) supports_union: bool,
    pub(crate) supports_negation: bool,
    pub(crate) default_top_k: usize,
    pub(crate) default_min_score: f64,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct OracleBinding {
    pub(crate) subject: String,
    pub(crate) graph_id: String,
    pub(crate) mode: String,
    pub(crate) allowed_tasks: Vec<String>,
    pub(crate) top_k: usize,
    pub(crate) min_score: f64,
    pub(crate) max_hops: usize,
    pub(crate) allow_path: bool,
    pub(crate) allow_intersection: bool,
    pub(crate) allow_union: bool,
    pub(crate) allow_negation: bool,
    pub(crate) structural_weight: f64,
    pub(crate) embedding_weight: f64,
    pub(crate) lexical_weight: f64,
    pub(crate) weights_normalized: bool,
    pub(crate) persist_policy: String,
    pub(crate) requires_acceptance: bool,
    pub(crate) max_persisted_candidates: usize,
}

static OMPHALOS_STORES: OnceLock<Mutex<BTreeMap<PathBuf, Arc<Store>>>> = OnceLock::new();

pub(crate) fn omphalos_path(app: &AppHandle) -> Result<PathBuf, String> {
    Ok(omphalos_path_from_profile_dir(&profile_dir(app)?))
}

fn omphalos_path_from_profile_dir(profile: &Path) -> PathBuf {
    if let Ok(path) = std::env::var("SOPHIA_OMPHALOS") {
        let trimmed = path.trim();
        if !trimmed.is_empty() {
            return PathBuf::from(trimmed);
        }
    }
    profile.join("omphalos")
}

fn constitution_path_for_omphalos(omphalos_path: &Path) -> PathBuf {
    omphalos_path.join(CONSTITUTION_FILE)
}

pub(crate) fn open_omphalos(app: &AppHandle) -> Result<Arc<Store>, String> {
    let store_path = omphalos_path(app)?;

    // The cached store was necessarily visible to any concurrent durability
    // enumeration. Do not make ordinary reads wait for the EFS file walk.
    if let Some(stores) = OMPHALOS_STORES.get() {
        let stores = stores
            .lock()
            .map_err(|_| "omphalos Oxigraph store cache lock poisoned".to_string())?;
        if store_path.is_dir() {
            if let Some(store) = stores.get(&store_path) {
                return Ok(Arc::clone(store));
            }
        }
    }

    let _lifecycle_guard = crate::rdf_store_service::rdf_store_lifecycle_read_guard()?;
    let store_was_present = store_path.is_dir();
    create_dir_all(&store_path).map_err(|error| {
        format!(
            "create omphalos directory {}: {error}",
            display_path(&store_path)
        )
    })?;
    let mut stores = OMPHALOS_STORES
        .get_or_init(|| Mutex::new(BTreeMap::new()))
        .lock()
        .map_err(|_| "omphalos Oxigraph store cache lock poisoned".to_string())?;

    if store_was_present {
        if let Some(store) = stores.get(&store_path) {
            return Ok(Arc::clone(store));
        }
    } else {
        stores.remove(&store_path);
    }

    let store =
        Arc::new(Store::open(&store_path).map_err(|error| {
            format!("open omphalos store {}: {error}", display_path(&store_path))
        })?);
    stores.insert(store_path, Arc::clone(&store));
    Ok(store)
}

/// Snapshot the currently-open Omphalos stores for the durable-plane
/// checkpoint pass. Omphalos uses the profile's `omphalos/` directory itself
/// as its RocksDB root (rather than a `*.oxigraph` name), so it must be listed
/// explicitly and skipped by exact path during the plain-file walk.
///
/// The caller must hold the RDF-store lifecycle gate exclusively; this helper
/// intentionally does not re-enter it.
#[cfg_attr(feature = "desktop", allow(dead_code))]
pub(crate) fn open_omphalos_stores() -> Vec<(PathBuf, Arc<Store>)> {
    let Some(stores) = OMPHALOS_STORES.get() else {
        return Vec::new();
    };
    match stores.lock() {
        Ok(stores) => stores
            .iter()
            .map(|(path, store)| (path.clone(), Arc::clone(store)))
            .collect(),
        Err(_) => Vec::new(),
    }
}

/// Establish the profile's omphalos constitution before local services start.
///
/// An authored `constitution.ttl` is always authoritative and is reconciled
/// through the same SHACL-gated path as every later read. A store that is
/// already constituted is likewise left alone when no file is present. Only a
/// genuinely empty, fresh store receives the compiled default.
pub(crate) fn initialize_constitution(app: &AppHandle) -> Result<(), String> {
    open_initialized_omphalos(app).map(|_| ())
}

fn open_initialized_omphalos(app: &AppHandle) -> Result<Arc<Store>, String> {
    let store_path = omphalos_path(app)?;
    let constitution_path = constitution_path_for_omphalos(&store_path);
    let store = open_omphalos(app)?;

    if constitution_path.is_file() {
        constitute_file_if_present(&store, &constitution_path)?;
    } else if store.is_empty().map_err(|error| {
        format!(
            "inspect omphalos store {}: {error}",
            display_path(&store_path)
        )
    })? {
        constitute_turtle(&store, DEFAULT_CONSTITUTION_TTL)?;
    }

    // SHACL validation protects file/default writes. This read also makes an
    // already-populated store without a constitution fail loudly instead of
    // silently replacing or accepting it.
    read_constitution(&store)?;
    Ok(store)
}

pub(crate) fn constitute(store: &Store, desired: &[Triple]) -> Result<TripleDiff, String> {
    let scopes_desireds = partition_constitution_triples(desired)?;
    reconcile_classes_validated(store, &scopes_desireds, Some(nomos_vocabulary()))
}

pub(crate) fn constitute_turtle(store: &Store, ttl: &str) -> Result<TripleDiff, String> {
    let desired = constitution_triples_from_turtle(ttl)?;
    constitute(store, &desired)
}

fn constitute_file_if_present(store: &Store, path: &Path) -> Result<(), String> {
    if !path.is_file() {
        return Ok(());
    }
    let ttl = std::fs::read_to_string(path)
        .map_err(|error| format!("read constitution {}: {error}", display_path(path)))?;
    constitute_turtle(store, &ttl)?;
    Ok(())
}

pub(crate) fn read_embedder_selection(app: &AppHandle) -> Result<EmbedderSelection, String> {
    let store = open_initialized_omphalos(app)?;
    Ok(read_constitution(&store)?.embedder)
}

pub(crate) fn read_oracle_constitution(
    app: &AppHandle,
    graph_id: &str,
    oracle_kind: &str,
) -> Result<Option<OracleConstitution>, String> {
    let store = open_initialized_omphalos(app)?;
    read_oracle_constitution_from_store(&store, graph_id, oracle_kind)
}

pub(crate) fn read_constitution(store: &Store) -> Result<Constitution, String> {
    let worlds = world_subjects(store)?;
    match worlds.as_slice() {
        [] => {
            return Err(
                "omphalos constitution is absent: no nomos:World found; seed constitution.ttl through the validated constitute path".to_string(),
            )
        }
        [world] if world == WORLD_SUBJECT => {}
        [world] => {
            return Err(format!(
                "omphalos constitution has unexpected nomos:World subject {world}; expected {WORLD_SUBJECT}"
            ))
        }
        _ => {
            return Err(format!(
                "omphalos constitution must contain exactly one nomos:World, found {}",
                worlds.len()
            ))
        }
    }

    let embedder_rows = select_solutions(
        store,
        &format!(
            "PREFIX nomos: <{NOMOS_NS}>
SELECT ?schema ?anchor ?provider ?model ?dim WHERE {{
  <{WORLD_SUBJECT}> nomos:schemaVersion ?schema ;
         nomos:identityAnchor ?anchor ;
         nomos:usesEmbedder ?emb .
  ?emb nomos:providerId ?provider ;
       nomos:modelId ?model ;
       nomos:dimensions ?dim .
}}"
        ),
    )?;
    let row = match embedder_rows.as_slice() {
        [row] => row,
        [] => return Err(
            "omphalos constitution is malformed: nomos:World has no complete embedder selection"
                .to_string(),
        ),
        _ => return Err(
            "omphalos constitution is malformed: nomos:World resolves multiple embedder selections"
                .to_string(),
        ),
    };

    let schema_version = literal_u32(row.get("schema"), "schema")?;
    let identity_anchor = literal_string(row.get("anchor"), "identityAnchor")?;
    let provider_id = literal_string(row.get("provider"), "providerId")?;
    let model_id = literal_string(row.get("model"), "modelId")?;
    let dimensions = literal_usize(row.get("dim"), "dimensions")?;

    let graph_rows = select_solutions(
        store,
        &format!(
            "PREFIX nomos: <{NOMOS_NS}>
SELECT ?graphId WHERE {{
  <{WORLD_SUBJECT}> nomos:hostsGraph ?g .
  ?g nomos:graphId ?graphId .
}}"
        ),
    )?;
    let mut graphs = Vec::new();
    for row in graph_rows {
        graphs.push(GraphEntry {
            graph_id: literal_string(row.get("graphId"), "graphId")?,
        });
    }

    Ok(Constitution {
        schema_version,
        identity_anchor,
        embedder: EmbedderSelection {
            provider_id,
            model_id,
            dimensions,
        },
        graphs,
    })
}

pub(crate) fn read_oracle_constitution_from_store(
    store: &Store,
    graph_id: &str,
    oracle_kind: &str,
) -> Result<Option<OracleConstitution>, String> {
    let graph_id = graph_id.trim();
    if graph_id.is_empty() {
        return Err("omphalos oracle lookup requires graphId".to_string());
    }
    let oracle_kind = oracle_kind.trim();
    if oracle_kind.is_empty() {
        return Err("omphalos oracle lookup requires oracleKind".to_string());
    }

    let oracle_rows = select_solutions(
        store,
        &format!(
            "PREFIX nomos: <{NOMOS_NS}>
SELECT ?oracle ?kind ?service ?enabled ?input ?output ?outputVersion ?projection WHERE {{
  <{WORLD_SUBJECT}> nomos:usesOracle ?oracle .
  ?oracle a nomos:Oracle ;
          nomos:oracleKind ?kind ;
          nomos:serviceId ?service ;
          nomos:enabled ?enabled ;
          nomos:inputLayer ?input ;
          nomos:outputVocab ?output ;
          nomos:projectionTarget ?projection .
  OPTIONAL {{ ?oracle nomos:outputVocabVersion ?outputVersion }}
}}"
        ),
    )?;
    let mut matching_oracles = Vec::new();
    for row in oracle_rows {
        if literal_string(row.get("kind"), "oracleKind")? == oracle_kind {
            matching_oracles.push(OracleSelection {
                subject: named_node_string(row.get("oracle"), "oracle")?,
                oracle_kind: oracle_kind.to_string(),
                service_id: literal_string(row.get("service"), "serviceId")?,
                enabled: literal_bool(row.get("enabled"), "enabled")?,
                input_layer: literal_string(row.get("input"), "inputLayer")?,
                output_vocab: literal_string(row.get("output"), "outputVocab")?,
                output_vocab_version: optional_literal_string(
                    row.get("outputVersion"),
                    "outputVocabVersion",
                )?,
                projection_target: literal_string(row.get("projection"), "projectionTarget")?,
            });
        }
    }
    let oracle = match matching_oracles.as_slice() {
        [] => return Ok(None),
        [oracle] => oracle.clone(),
        _ => {
            return Err(format!(
                "omphalos constitution is malformed: multiple nomos:Oracle entries for kind {oracle_kind}"
            ))
        }
    };

    let tasks = read_oracle_tasks(store, &oracle.subject)?;
    let binding = match read_oracle_binding(store, &oracle.subject, graph_id)? {
        Some(binding) => binding,
        None => return Ok(None),
    };
    validate_oracle_binding(&tasks, &binding)?;

    Ok(Some(OracleConstitution {
        oracle,
        tasks,
        binding,
    }))
}

fn read_oracle_tasks(
    store: &Store,
    oracle_subject: &str,
) -> Result<Vec<OracleTaskSelection>, String> {
    let task_rows = select_solutions(
        store,
        &format!(
            "PREFIX nomos: <{NOMOS_NS}>
SELECT ?task ?taskId ?taskKind ?model ?modelVersion ?supportsPath ?supportsIntersection ?supportsUnion ?supportsNegation ?defaultTopK ?defaultMinScore WHERE {{
  <{oracle_subject}> nomos:supportsTask ?task .
  ?task a nomos:OracleTask ;
        nomos:taskId ?taskId ;
        nomos:taskKind ?taskKind ;
        nomos:modelId ?model .
  OPTIONAL {{ ?task nomos:modelVersion ?modelVersion }}
  OPTIONAL {{ ?task nomos:supportsPath ?supportsPath }}
  OPTIONAL {{ ?task nomos:supportsIntersection ?supportsIntersection }}
  OPTIONAL {{ ?task nomos:supportsUnion ?supportsUnion }}
  OPTIONAL {{ ?task nomos:supportsNegation ?supportsNegation }}
  OPTIONAL {{ ?task nomos:defaultTopK ?defaultTopK }}
  OPTIONAL {{ ?task nomos:defaultMinScore ?defaultMinScore }}
}}"
        ),
    )?;
    let mut tasks = Vec::new();
    for row in task_rows {
        tasks.push(OracleTaskSelection {
            subject: named_node_string(row.get("task"), "task")?,
            task_id: literal_string(row.get("taskId"), "taskId")?,
            task_kind: literal_string(row.get("taskKind"), "taskKind")?,
            model_id: literal_string(row.get("model"), "modelId")?,
            model_version: optional_literal_string(row.get("modelVersion"), "modelVersion")?,
            supports_path: optional_literal_bool(row.get("supportsPath"), "supportsPath", false)?,
            supports_intersection: optional_literal_bool(
                row.get("supportsIntersection"),
                "supportsIntersection",
                false,
            )?,
            supports_union: optional_literal_bool(
                row.get("supportsUnion"),
                "supportsUnion",
                false,
            )?,
            supports_negation: optional_literal_bool(
                row.get("supportsNegation"),
                "supportsNegation",
                false,
            )?,
            default_top_k: optional_literal_usize(row.get("defaultTopK"), "defaultTopK", 50)?,
            default_min_score: optional_literal_f64(
                row.get("defaultMinScore"),
                "defaultMinScore",
                0.20,
            )?,
        });
    }
    tasks.sort_by(|a, b| a.task_id.cmp(&b.task_id));
    Ok(tasks)
}

#[derive(Debug, Clone)]
struct BindingDraft {
    subject: String,
    graph_id: String,
    mode: Option<String>,
    allowed_tasks: BTreeSet<String>,
    top_k: Option<usize>,
    min_score: Option<f64>,
    max_hops: Option<usize>,
    allow_path: Option<bool>,
    allow_intersection: Option<bool>,
    allow_union: Option<bool>,
    allow_negation: Option<bool>,
    structural_weight: Option<f64>,
    embedding_weight: Option<f64>,
    lexical_weight: Option<f64>,
    persist_policy: Option<String>,
    requires_acceptance: Option<bool>,
    max_persisted_candidates: Option<usize>,
}

fn read_oracle_binding(
    store: &Store,
    oracle_subject: &str,
    graph_id: &str,
) -> Result<Option<OracleBinding>, String> {
    let binding_rows = select_solutions(
        store,
        &format!(
            "PREFIX nomos: <{NOMOS_NS}>
SELECT ?binding ?graphId ?mode ?allowedTask ?topK ?minScore ?maxHops ?allowPath ?allowIntersection ?allowUnion ?allowNegation ?structuralWeight ?embeddingWeight ?lexicalWeight ?persistPolicy ?requiresAcceptance ?maxPersistedCandidates WHERE {{
  <{WORLD_SUBJECT}> nomos:hasOracleBinding ?binding .
  ?binding a nomos:OracleBinding ;
           nomos:usesOracle <{oracle_subject}> ;
           nomos:graphId ?graphId .
  OPTIONAL {{ ?binding nomos:mode ?mode }}
  OPTIONAL {{ ?binding nomos:allowedTask ?allowedTask }}
  OPTIONAL {{ ?binding nomos:topK ?topK }}
  OPTIONAL {{ ?binding nomos:minScore ?minScore }}
  OPTIONAL {{ ?binding nomos:maxHops ?maxHops }}
  OPTIONAL {{ ?binding nomos:allowPath ?allowPath }}
  OPTIONAL {{ ?binding nomos:allowIntersection ?allowIntersection }}
  OPTIONAL {{ ?binding nomos:allowUnion ?allowUnion }}
  OPTIONAL {{ ?binding nomos:allowNegation ?allowNegation }}
  OPTIONAL {{ ?binding nomos:structuralWeight ?structuralWeight }}
  OPTIONAL {{ ?binding nomos:embeddingWeight ?embeddingWeight }}
  OPTIONAL {{ ?binding nomos:lexicalWeight ?lexicalWeight }}
  OPTIONAL {{ ?binding nomos:persistPolicy ?persistPolicy }}
  OPTIONAL {{ ?binding nomos:requiresAcceptance ?requiresAcceptance }}
  OPTIONAL {{ ?binding nomos:maxPersistedCandidates ?maxPersistedCandidates }}
}}"
        ),
    )?;

    let mut drafts: BTreeMap<String, BindingDraft> = BTreeMap::new();
    for row in binding_rows {
        let subject = named_node_string(row.get("binding"), "binding")?;
        let graph = literal_string(row.get("graphId"), "graphId")?;
        let draft = drafts
            .entry(subject.clone())
            .or_insert_with(|| BindingDraft {
                subject,
                graph_id: graph.clone(),
                mode: None,
                allowed_tasks: BTreeSet::new(),
                top_k: None,
                min_score: None,
                max_hops: None,
                allow_path: None,
                allow_intersection: None,
                allow_union: None,
                allow_negation: None,
                structural_weight: None,
                embedding_weight: None,
                lexical_weight: None,
                persist_policy: None,
                requires_acceptance: None,
                max_persisted_candidates: None,
            });
        if draft.graph_id != graph {
            return Err(format!(
                "omphalos OracleBinding {} has conflicting graphId values",
                draft.subject
            ));
        }
        merge_optional_string(&mut draft.mode, row.get("mode"), "mode")?;
        if let Some(task) = optional_literal_string(row.get("allowedTask"), "allowedTask")? {
            draft.allowed_tasks.insert(task);
        }
        merge_optional_usize(&mut draft.top_k, row.get("topK"), "topK")?;
        merge_optional_f64(&mut draft.min_score, row.get("minScore"), "minScore")?;
        merge_optional_usize(&mut draft.max_hops, row.get("maxHops"), "maxHops")?;
        merge_optional_bool(&mut draft.allow_path, row.get("allowPath"), "allowPath")?;
        merge_optional_bool(
            &mut draft.allow_intersection,
            row.get("allowIntersection"),
            "allowIntersection",
        )?;
        merge_optional_bool(&mut draft.allow_union, row.get("allowUnion"), "allowUnion")?;
        merge_optional_bool(
            &mut draft.allow_negation,
            row.get("allowNegation"),
            "allowNegation",
        )?;
        merge_optional_f64(
            &mut draft.structural_weight,
            row.get("structuralWeight"),
            "structuralWeight",
        )?;
        merge_optional_f64(
            &mut draft.embedding_weight,
            row.get("embeddingWeight"),
            "embeddingWeight",
        )?;
        merge_optional_f64(
            &mut draft.lexical_weight,
            row.get("lexicalWeight"),
            "lexicalWeight",
        )?;
        merge_optional_string(
            &mut draft.persist_policy,
            row.get("persistPolicy"),
            "persistPolicy",
        )?;
        merge_optional_bool(
            &mut draft.requires_acceptance,
            row.get("requiresAcceptance"),
            "requiresAcceptance",
        )?;
        merge_optional_usize(
            &mut draft.max_persisted_candidates,
            row.get("maxPersistedCandidates"),
            "maxPersistedCandidates",
        )?;
    }

    let exact = drafts
        .values()
        .filter(|draft| draft.graph_id == graph_id)
        .collect::<Vec<_>>();
    let wildcard = drafts
        .values()
        .filter(|draft| draft.graph_id == "*")
        .collect::<Vec<_>>();
    let selected = if !exact.is_empty() { exact } else { wildcard };
    let draft = match selected.as_slice() {
        [] => return Ok(None),
        [draft] => *draft,
        _ => {
            return Err(format!(
                "omphalos constitution is malformed: multiple OracleBinding entries match graphId {graph_id}"
            ))
        }
    };

    binding_from_draft(draft)
}

fn binding_from_draft(draft: &BindingDraft) -> Result<Option<OracleBinding>, String> {
    let mut structural_weight = draft.structural_weight.unwrap_or(0.55);
    let mut embedding_weight = draft.embedding_weight.unwrap_or(0.35);
    let mut lexical_weight = draft.lexical_weight.unwrap_or(0.10);
    let weight_sum = structural_weight + embedding_weight + lexical_weight;
    if weight_sum <= 0.0 {
        return Err(format!(
            "omphalos OracleBinding {} has non-positive candidate scoring weights",
            draft.subject
        ));
    }
    let weights_normalized = (weight_sum - 1.0).abs() > 0.000_001;
    if weights_normalized {
        structural_weight /= weight_sum;
        embedding_weight /= weight_sum;
        lexical_weight /= weight_sum;
    }

    Ok(Some(OracleBinding {
        subject: draft.subject.clone(),
        graph_id: draft.graph_id.clone(),
        mode: draft.mode.clone().unwrap_or_else(|| "active".to_string()),
        allowed_tasks: draft.allowed_tasks.iter().cloned().collect(),
        top_k: draft.top_k.unwrap_or(50),
        min_score: draft.min_score.unwrap_or(0.20),
        max_hops: draft.max_hops.unwrap_or(3),
        allow_path: draft.allow_path.unwrap_or(true),
        allow_intersection: draft.allow_intersection.unwrap_or(true),
        allow_union: draft.allow_union.unwrap_or(true),
        allow_negation: draft.allow_negation.unwrap_or(false),
        structural_weight,
        embedding_weight,
        lexical_weight,
        weights_normalized,
        persist_policy: draft
            .persist_policy
            .clone()
            .unwrap_or_else(|| "on-request".to_string()),
        requires_acceptance: draft.requires_acceptance.unwrap_or(true),
        max_persisted_candidates: draft.max_persisted_candidates.unwrap_or(25),
    }))
}

fn validate_oracle_binding(
    tasks: &[OracleTaskSelection],
    binding: &OracleBinding,
) -> Result<(), String> {
    let task_ids = tasks
        .iter()
        .map(|task| task.task_id.as_str())
        .collect::<BTreeSet<_>>();
    for allowed in &binding.allowed_tasks {
        if !task_ids.contains(allowed.as_str()) {
            return Err(format!(
                "omphalos OracleBinding {} allows unknown taskId {allowed}",
                binding.subject
            ));
        }
    }
    let effective_tasks = tasks
        .iter()
        .filter(|task| {
            binding.allowed_tasks.is_empty() || binding.allowed_tasks.contains(&task.task_id)
        })
        .collect::<Vec<_>>();
    if effective_tasks.is_empty() {
        return Err(format!(
            "omphalos OracleBinding {} has no usable OracleTask",
            binding.subject
        ));
    }
    for task in effective_tasks
        .iter()
        .copied()
        .filter(|task| task.task_kind == "logical-query-answering")
    {
        if binding.allow_path && !task.supports_path {
            return Err(format!(
                "omphalos OracleBinding {} enables path queries, but task {} does not support them",
                binding.subject, task.task_id
            ));
        }
        if binding.allow_intersection && !task.supports_intersection {
            return Err(format!(
                "omphalos OracleBinding {} enables intersection queries, but task {} does not support them",
                binding.subject, task.task_id
            ));
        }
        if binding.allow_union && !task.supports_union {
            return Err(format!(
                "omphalos OracleBinding {} enables union queries, but task {} does not support them",
                binding.subject, task.task_id
            ));
        }
        if binding.allow_negation && !task.supports_negation {
            return Err(format!(
                "omphalos OracleBinding {} enables negation, but task {} does not support it",
                binding.subject, task.task_id
            ));
        }
    }
    if binding.allow_negation && !effective_tasks.iter().any(|task| task.supports_negation) {
        return Err(format!(
            "omphalos OracleBinding {} enables negation, but no allowed task supports it",
            binding.subject
        ));
    }
    Ok(())
}

pub(crate) fn constitution_triples_from_turtle(ttl: &str) -> Result<Vec<Triple>, String> {
    let temp =
        Store::new().map_err(|error| format!("create temporary RDF parser store: {error}"))?;
    temp.load_from_slice(RdfParser::from_format(RdfFormat::Turtle), ttl.as_bytes())
        .map_err(|error| format!("parse constitution Turtle: {error}"))?;
    let rows = select_solutions(&temp, "SELECT ?s ?p ?o WHERE { ?s ?p ?o }")?;
    let mut triples = Vec::new();
    for row in rows {
        let s = named_node_string(row.get("s"), "s")?;
        let p = named_node_string(row.get("p"), "p")?;
        let o = row
            .get("o")
            .ok_or_else(|| "constitution parse row missing ?o".to_string())
            .map(|term| parse_term(&term.to_string()))?;
        triples.push((s, p, o));
    }
    triples.sort_by(|a, b| {
        a.0.cmp(&b.0)
            .then_with(|| a.1.cmp(&b.1))
            .then_with(|| a.2.as_nt().cmp(&b.2.as_nt()))
    });
    Ok(triples)
}

fn partition_constitution_triples(
    desired: &[Triple],
) -> Result<Vec<(ClassScope, Vec<Triple>)>, String> {
    let mut subjects_by_type: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for (subject, predicate, object) in desired {
        if predicate == RDF_TYPE {
            if let Term::Uri(node) = object {
                subjects_by_type
                    .entry(node.as_str().to_string())
                    .or_default()
                    .insert(subject.clone());
            }
        }
    }

    [
        world_scope(),
        embedder_scope(),
        graph_scope(),
        oracle_scope(),
        oracle_task_scope(),
        oracle_binding_scope(),
    ]
    .into_iter()
    .map(|scope| {
        let SpanKey::Fixed { rdf_type } = &scope.key;
        let Some(subjects) = subjects_by_type.get(rdf_type) else {
            return Ok((scope, Vec::new()));
        };
        let subset = desired
            .iter()
            .filter(|(subject, _, _)| subjects.contains(subject))
            .cloned()
            .collect::<Vec<_>>();
        Ok((scope, subset))
    })
    .collect()
}

fn world_scope() -> ClassScope {
    class_scope("World")
}

fn embedder_scope() -> ClassScope {
    class_scope("Embedder")
}

fn graph_scope() -> ClassScope {
    class_scope("Graph")
}

fn oracle_scope() -> ClassScope {
    class_scope("Oracle")
}

fn oracle_task_scope() -> ClassScope {
    class_scope("OracleTask")
}

fn oracle_binding_scope() -> ClassScope {
    class_scope("OracleBinding")
}

fn class_scope(local: &str) -> ClassScope {
    ClassScope {
        placement: Placement::Default,
        key: SpanKey::Fixed {
            rdf_type: format!("{NOMOS_NS}{local}"),
        },
        graph_id_conjunct: None,
        subjects: None,
    }
}

fn world_subjects(store: &Store) -> Result<Vec<String>, String> {
    let rows = select_solutions(
        store,
        &format!("PREFIX nomos: <{NOMOS_NS}> SELECT ?world WHERE {{ ?world a nomos:World }}"),
    )?;
    let mut worlds = Vec::new();
    for row in rows {
        worlds.push(named_node_string(row.get("world"), "world")?);
    }
    worlds.sort();
    worlds.dedup();
    Ok(worlds)
}

fn select_solutions(
    store: &Store,
    query: &str,
) -> Result<Vec<BTreeMap<String, oxigraph::model::Term>>, String> {
    let solutions = match SparqlEvaluator::new()
        .parse_query(query)
        .map_err(|error| format!("parse omphalos query: {error}"))?
        .on_store(store)
        .execute()
        .map_err(|error| format!("execute omphalos query: {error}"))?
    {
        QueryResults::Solutions(solutions) => solutions,
        _ => return Err("omphalos query expected SELECT solutions".to_string()),
    };

    let mut rows = Vec::new();
    for solution in solutions {
        let solution = solution.map_err(|error| format!("read omphalos query row: {error}"))?;
        let mut row = BTreeMap::new();
        for (name, term) in solution.iter() {
            row.insert(
                name.to_string().trim_start_matches('?').to_string(),
                term.clone(),
            );
        }
        rows.push(row);
    }
    Ok(rows)
}

fn named_node_string(term: Option<&oxigraph::model::Term>, field: &str) -> Result<String, String> {
    match term {
        Some(oxigraph::model::Term::NamedNode(node)) => Ok(node.as_str().to_string()),
        Some(other) => Err(format!("omphalos field ?{field} expected IRI, got {other}")),
        None => Err(format!("omphalos row missing ?{field}")),
    }
}

fn literal_string(term: Option<&oxigraph::model::Term>, field: &str) -> Result<String, String> {
    match term {
        Some(oxigraph::model::Term::Literal(literal)) => Ok(literal.value().to_string()),
        Some(other) => Err(format!(
            "omphalos field ?{field} expected literal, got {other}"
        )),
        None => Err(format!("omphalos row missing ?{field}")),
    }
}

fn literal_u32(term: Option<&oxigraph::model::Term>, field: &str) -> Result<u32, String> {
    literal_string(term, field)?
        .parse::<u32>()
        .map_err(|error| format!("omphalos field ?{field} expected u32: {error}"))
}

fn literal_usize(term: Option<&oxigraph::model::Term>, field: &str) -> Result<usize, String> {
    literal_string(term, field)?
        .parse::<usize>()
        .map_err(|error| format!("omphalos field ?{field} expected usize: {error}"))
}

fn literal_bool(term: Option<&oxigraph::model::Term>, field: &str) -> Result<bool, String> {
    literal_string(term, field)?
        .parse::<bool>()
        .map_err(|error| format!("omphalos field ?{field} expected bool: {error}"))
}

fn literal_f64(term: Option<&oxigraph::model::Term>, field: &str) -> Result<f64, String> {
    literal_string(term, field)?
        .parse::<f64>()
        .map_err(|error| format!("omphalos field ?{field} expected f64: {error}"))
}

fn optional_literal_string(
    term: Option<&oxigraph::model::Term>,
    field: &str,
) -> Result<Option<String>, String> {
    term.map(|term| literal_string(Some(term), field))
        .transpose()
}

fn optional_literal_bool(
    term: Option<&oxigraph::model::Term>,
    field: &str,
    default: bool,
) -> Result<bool, String> {
    term.map(|term| literal_bool(Some(term), field))
        .transpose()
        .map(|value| value.unwrap_or(default))
}

fn optional_literal_usize(
    term: Option<&oxigraph::model::Term>,
    field: &str,
    default: usize,
) -> Result<usize, String> {
    term.map(|term| literal_usize(Some(term), field))
        .transpose()
        .map(|value| value.unwrap_or(default))
}

fn optional_literal_f64(
    term: Option<&oxigraph::model::Term>,
    field: &str,
    default: f64,
) -> Result<f64, String> {
    term.map(|term| literal_f64(Some(term), field))
        .transpose()
        .map(|value| value.unwrap_or(default))
}

fn merge_optional_string(
    slot: &mut Option<String>,
    term: Option<&oxigraph::model::Term>,
    field: &str,
) -> Result<(), String> {
    if let Some(value) = optional_literal_string(term, field)? {
        merge_scalar(slot, value, field)?;
    }
    Ok(())
}

fn merge_optional_bool(
    slot: &mut Option<bool>,
    term: Option<&oxigraph::model::Term>,
    field: &str,
) -> Result<(), String> {
    if let Some(term) = term {
        merge_scalar(slot, literal_bool(Some(term), field)?, field)?;
    }
    Ok(())
}

fn merge_optional_usize(
    slot: &mut Option<usize>,
    term: Option<&oxigraph::model::Term>,
    field: &str,
) -> Result<(), String> {
    if let Some(term) = term {
        merge_scalar(slot, literal_usize(Some(term), field)?, field)?;
    }
    Ok(())
}

fn merge_optional_f64(
    slot: &mut Option<f64>,
    term: Option<&oxigraph::model::Term>,
    field: &str,
) -> Result<(), String> {
    if let Some(term) = term {
        merge_scalar(slot, literal_f64(Some(term), field)?, field)?;
    }
    Ok(())
}

fn merge_scalar<T: PartialEq + std::fmt::Display>(
    slot: &mut Option<T>,
    value: T,
    field: &str,
) -> Result<(), String> {
    match slot {
        Some(existing) if existing != &value => Err(format!(
            "omphalos OracleBinding has conflicting ?{field} values {existing} and {value}"
        )),
        Some(_) => Ok(()),
        None => {
            *slot = Some(value);
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use oxigraph::model::{Literal, NamedNode};
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_path(name: &str) -> PathBuf {
        let suffix = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        std::env::temp_dir().join(format!("garden-omphalos-{name}-{suffix}"))
    }

    fn parsed_default() -> Vec<Triple> {
        constitution_triples_from_turtle(DEFAULT_CONSTITUTION_TTL)
            .expect("default constitution parses")
    }

    fn triple_count(store: &Store) -> usize {
        select_solutions(store, "SELECT ?s ?p ?o WHERE { ?s ?p ?o }")
            .expect("count triples")
            .len()
    }

    #[test]
    fn omphalos_path_resolves_from_env_else_profile() {
        let profile = temp_path("profile");
        std::env::remove_var("SOPHIA_OMPHALOS");
        assert_eq!(
            omphalos_path_from_profile_dir(&profile),
            profile.join("omphalos")
        );

        let explicit = temp_path("explicit");
        std::env::set_var("SOPHIA_OMPHALOS", &explicit);
        assert_eq!(omphalos_path_from_profile_dir(&profile), explicit);
        std::env::remove_var("SOPHIA_OMPHALOS");
    }

    #[test]
    fn omphalos_constitution_roundtrip() {
        let store = Store::new().expect("in-memory store");
        let diff = constitute(&store, &parsed_default()).expect("constitute default");
        assert!(diff.op_count() > 0);

        let constitution = read_constitution(&store).expect("read constitution");
        assert_eq!(constitution.schema_version, 2);
        assert_eq!(constitution.identity_anchor, "rdf-iri");
        assert_eq!(constitution.embedder.provider_id, "fastembed");
        assert_eq!(
            constitution.embedder.model_id,
            "fastembed/qdrant/bge-small-en-v1.5-onnx-q"
        );
        assert_eq!(constitution.embedder.dimensions, 384);
        assert_eq!(
            constitution.graphs,
            vec![GraphEntry {
                graph_id: "default".to_string()
            }]
        );
    }

    #[test]
    fn omphalos_default_oracle_binding_is_readable() {
        let store = Store::new().expect("in-memory store");
        constitute(&store, &parsed_default()).expect("constitute default");

        let oracle = read_oracle_constitution_from_store(&store, "any-graph", "kg-ultra")
            .expect("read oracle")
            .expect("default oracle binding");

        assert_eq!(oracle.oracle.service_id, "kg-ultra");
        assert!(oracle.oracle.enabled);
        assert_eq!(oracle.binding.graph_id, "*");
        assert_eq!(oracle.binding.allowed_tasks.len(), 2);
        assert_eq!(oracle.binding.top_k, 50);
        assert_eq!(oracle.binding.max_hops, 3);
        assert!(!oracle.binding.allow_negation);
        assert!(!oracle.binding.weights_normalized);
        assert!(oracle
            .tasks
            .iter()
            .any(|task| task.task_id == "link-prediction"));
        assert!(oracle
            .tasks
            .iter()
            .any(|task| task.task_id == "logical-query-answering"));
    }

    #[test]
    fn omphalos_oracle_binding_exact_graph_overrides_wildcard() {
        let store = Store::new().expect("in-memory store");
        let ttl = format!(
            "{DEFAULT_CONSTITUTION_TTL}

<urn:mnemosyne:local:world>
  nomos:hasOracleBinding <urn:mnemosyne:local:oracle-binding:kg-ultra:lab> .

<urn:mnemosyne:local:oracle-binding:kg-ultra:lab>
  a nomos:OracleBinding ;
  nomos:usesOracle <urn:mnemosyne:local:oracle:kg-ultra> ;
  nomos:graphId \"lab\" ;
  nomos:mode \"active\" ;
  nomos:allowedTask \"link-prediction\" ;
  nomos:topK 7 ;
  nomos:minScore \"0.42\"^^xsd:double ;
  nomos:structuralWeight \"3.0\"^^xsd:double ;
  nomos:embeddingWeight \"1.0\"^^xsd:double ;
  nomos:lexicalWeight \"0.0\"^^xsd:double ."
        );
        constitute_turtle(&store, &ttl).expect("constitute override");

        let oracle = read_oracle_constitution_from_store(&store, "lab", "kg-ultra")
            .expect("read oracle")
            .expect("exact oracle binding");

        assert_eq!(oracle.binding.graph_id, "lab");
        assert_eq!(oracle.binding.allowed_tasks, vec!["link-prediction"]);
        assert_eq!(oracle.binding.top_k, 7);
        assert_eq!(oracle.binding.min_score, 0.42);
        assert!(oracle.binding.weights_normalized);
        assert_eq!(oracle.binding.structural_weight, 0.75);
        assert_eq!(oracle.binding.embedding_weight, 0.25);
        assert_eq!(oracle.binding.lexical_weight, 0.0);
    }

    #[test]
    fn omphalos_oracle_binding_rejects_unknown_task() {
        let store = Store::new().expect("in-memory store");
        let ttl = format!(
            "{DEFAULT_CONSTITUTION_TTL}

<urn:mnemosyne:local:world>
  nomos:hasOracleBinding <urn:mnemosyne:local:oracle-binding:kg-ultra:bad-task> .

<urn:mnemosyne:local:oracle-binding:kg-ultra:bad-task>
  a nomos:OracleBinding ;
  nomos:usesOracle <urn:mnemosyne:local:oracle:kg-ultra> ;
  nomos:graphId \"bad-task\" ;
  nomos:allowedTask \"does-not-exist\" ."
        );
        constitute_turtle(&store, &ttl).expect("constitute bad task binding");

        let error = read_oracle_constitution_from_store(&store, "bad-task", "kg-ultra")
            .expect_err("unknown task should halt");
        assert!(error.contains("unknown taskId does-not-exist"));
    }

    #[test]
    fn omphalos_oracle_binding_rejects_unsupported_negation() {
        let store = Store::new().expect("in-memory store");
        let ttl = format!(
            "{DEFAULT_CONSTITUTION_TTL}

<urn:mnemosyne:local:world>
  nomos:hasOracleBinding <urn:mnemosyne:local:oracle-binding:kg-ultra:negation> .

<urn:mnemosyne:local:oracle-binding:kg-ultra:negation>
  a nomos:OracleBinding ;
  nomos:usesOracle <urn:mnemosyne:local:oracle:kg-ultra> ;
  nomos:graphId \"negation\" ;
  nomos:allowedTask \"logical-query-answering\" ;
  nomos:allowNegation true ."
        );
        constitute_turtle(&store, &ttl).expect("constitute negation binding");

        let error = read_oracle_constitution_from_store(&store, "negation", "kg-ultra")
            .expect_err("unsupported negation should halt");
        assert!(error.contains("enables negation"));
    }

    #[test]
    fn omphalos_missing_oracle_is_unavailable_not_malformed() {
        let store = Store::new().expect("in-memory store");
        let ttl = r#"@prefix nomos: <http://mnemosyne.dev/nomos#> .
@prefix xsd:   <http://www.w3.org/2001/XMLSchema#> .

<urn:mnemosyne:local:world>
  a nomos:World ;
  nomos:schemaVersion 2 ;
  nomos:identityAnchor "rdf-iri" ;
  nomos:usesEmbedder <urn:mnemosyne:local:embedder:bge-small-en-v1.5-q> .

<urn:mnemosyne:local:embedder:bge-small-en-v1.5-q>
  a nomos:Embedder ;
  nomos:providerId "fastembed" ;
  nomos:modelId "fastembed/qdrant/bge-small-en-v1.5-onnx-q" ;
  nomos:dimensions 384 .
"#;
        constitute_turtle(&store, ttl).expect("constitute without oracle");
        let oracle = read_oracle_constitution_from_store(&store, "lab", "kg-ultra")
            .expect("read missing oracle");
        assert!(oracle.is_none());
    }

    #[test]
    fn omphalos_malformed_constitution_halts() {
        let store = Store::new().expect("in-memory store");
        constitute(&store, &parsed_default()).expect("initial constitution");
        let before = triple_count(&store);
        let malformed = parsed_default()
            .into_iter()
            .filter(|(_, predicate, _)| predicate != &format!("{NOMOS_NS}dimensions"))
            .collect::<Vec<_>>();

        let error = constitute(&store, &malformed).unwrap_err();
        assert!(error.contains("SHACL"), "expected SHACL halt, got {error}");
        assert_eq!(triple_count(&store), before, "store must stay unchanged");
    }

    #[test]
    fn omphalos_constitute_idempotent() {
        let store = Store::new().expect("in-memory store");
        let desired = parsed_default();
        let first = constitute(&store, &desired).expect("first constitute");
        let second = constitute(&store, &desired).expect("second constitute");

        assert!(first.op_count() > 0);
        assert_eq!(second.op_count(), 0);
    }

    #[test]
    fn omphalos_unconstituted_is_loud() {
        let store = Store::new().expect("in-memory store");
        let error = read_constitution(&store).unwrap_err();
        assert!(error.contains("constitution is absent"));
    }

    #[test]
    fn omphalos_two_worlds_are_loud() {
        let store = Store::new().expect("in-memory store");
        let mut desired = parsed_default();
        let other = "urn:mnemosyne:local:world:other".to_string();
        desired.push((
            other.clone(),
            RDF_TYPE.to_string(),
            Term::Uri(NamedNode::new(format!("{NOMOS_NS}World")).unwrap()),
        ));
        desired.push((
            other.clone(),
            format!("{NOMOS_NS}schemaVersion"),
            Term::Lit(Literal::new_typed_literal(
                "1",
                NamedNode::new("http://www.w3.org/2001/XMLSchema#integer").unwrap(),
            )),
        ));
        desired.push((
            other.clone(),
            format!("{NOMOS_NS}identityAnchor"),
            Term::Lit(Literal::new_simple_literal("rdf-iri")),
        ));
        desired.push((
            other,
            format!("{NOMOS_NS}usesEmbedder"),
            Term::Uri(NamedNode::new("urn:mnemosyne:local:embedder:bge-small-en-v1.5-q").unwrap()),
        ));

        constitute(&store, &desired).expect("SHACL accepts two structurally valid worlds");
        let error = read_constitution(&store).unwrap_err();
        assert!(error.contains("exactly one nomos:World"));
    }
}

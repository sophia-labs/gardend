use crate::app_runtime::AppHandle;
use crate::{
    clock::timestamp,
    crdt_projection_flush::flush_graph_projection,
    document_service::read_graph_documents_cold,
    graph_service::touch_graph_updated_at,
    paths::{existing_graph_dir, semantic_index_path},
    semantic_embedder::embed_texts_with_progress,
    semantic_index::{
        normalize_vector, read_semantic_index, semantic_entities_from_blocks, semantic_vector_ref,
        write_semantic_index, SemanticBlockEmbedding, SemanticBlockSource, SemanticIndexFile,
        SemanticIndexManifest, SemanticIndexStatus,
    },
    semantic_index_progress::SemanticIndexProgressSink,
    semantic_index_status::semantic_index_status,
    semantic_model_remote::remote_embeddings_endpoint,
    semantic_models::{
        read_semantic_model_config, semantic_model_effective_batch_size, semantic_model_spec_by_id,
        SEMANTIC_INDEX_SCHEMA_VERSION,
    },
    semantic_relation::{
        build_semantic_relations, collect_wire_relations, relation_profile_specs,
        write_semantic_relations, SemanticRelationConfig,
    },
    semantic_scaffold::{
        build_semantic_scaffold, semantic_scaffold_max_entities, write_semantic_scaffold,
        SemanticScaffoldConfig,
    },
    semantic_scaffold_rdf::reconcile_semantic_projection,
    semantic_search_projection::semantic_block_sources,
    storage::display_path,
};
use std::collections::BTreeMap;

pub(super) use crate::semantic_index_refresh_jobs::{
    submit_semantic_index_refresh_job, RefreshSemanticIndexInput, SemanticRefreshFlushBoundary,
};

pub(super) const SEMANTIC_REFRESH_STAGES: &[&str] = &[
    "validating",
    "collecting",
    "embedding",
    "neighbors",
    "clustering",
    "writing",
];

fn check_semantic_index_cancelled(
    progress: Option<&SemanticIndexProgressSink>,
) -> Result<(), String> {
    if let Some(sink) = progress {
        if sink.is_cancelled()? {
            return Err("semantic index refresh cancelled".to_string());
        }
    }
    Ok(())
}

pub(super) fn refresh_semantic_index_with_progress(
    app: &AppHandle,
    graph_id: &str,
    flush_boundary: SemanticRefreshFlushBoundary,
    progress: Option<&SemanticIndexProgressSink>,
) -> Result<SemanticIndexStatus, String> {
    if let Some(sink) = progress {
        sink.report(
            "validating",
            "Validating semantic refresh inputs",
            0,
            0,
            serde_json::json!({
                "flushBoundary": flush_boundary.as_str(),
                "stages": SEMANTIC_REFRESH_STAGES,
            }),
        )?;
    }
    honor_flush_boundary(app, graph_id, flush_boundary)?;
    let config = read_semantic_model_config(app)?;
    let spec = semantic_model_spec_by_id(&config.selected_model_id)?;
    // Redundant, independent re-check of the omphalos embedder dimensions —
    // provably redundant on the local path (the same check already happens
    // inside read_semantic_model_config's call chain), but it's a separate
    // omphalos touch that must be gated off independently on the remote
    // path, or this alone would error on a cell with no constitution.
    if remote_embeddings_endpoint().is_none() {
        let constitution_embedder = crate::omphalos::read_embedder_selection(app)?;
        if constitution_embedder.dimensions != spec.dimensions {
            return Err(format!(
                "omphalos embedder dimensions {} do not match active semantic model dimensions {}",
                constitution_embedder.dimensions, spec.dimensions
            ));
        }
    }
    let batch_size = semantic_model_effective_batch_size(&spec, &config);
    check_semantic_index_cancelled(progress)?;
    if let Some(sink) = progress {
        sink.report(
            "collecting",
            "Reading local documents",
            0,
            0,
            serde_json::json!({}),
        )?;
    }

    let graph_dir = existing_graph_dir(app, graph_id)?;
    let documents = read_graph_documents_cold(&graph_dir)?;
    check_semantic_index_cancelled(progress)?;

    if let Some(sink) = progress {
        sink.report(
            "collecting",
            "Projecting document blocks",
            0,
            documents.len(),
            serde_json::json!({ "documents": documents.len() }),
        )?;
    }
    let sources = semantic_block_sources(&documents);
    let source_count = sources.len();
    let previous_index = read_semantic_index(&graph_dir).ok().filter(|index| {
        index.manifest.model_id == spec.model_id && index.manifest.dimensions == spec.dimensions
    });
    let previous_by_iri = previous_blocks_by_iri(previous_index.as_ref());
    let mut blocks = Vec::with_capacity(sources.len());
    let mut embed_queue = Vec::<(usize, SemanticBlockSource)>::new();
    for source in sources {
        if let Some(block) =
            reusable_previous_block(&source, &previous_by_iri, spec.dimensions, blocks.len())
        {
            blocks.push(block);
        } else {
            let index = blocks.len();
            blocks.push(SemanticBlockEmbedding {
                iri: source.iri.clone(),
                kind: source.kind.clone(),
                graph_id: source.graph_id.clone(),
                document_id: source.document_id.clone(),
                document_title: source.document_title.clone(),
                block_id: source.block_id.clone(),
                block_type: source.block_type.clone(),
                content: source.content.clone(),
                content_hash: source.content_hash.clone(),
                order: source.order,
                vector: Vec::new(),
                vector_ref: Some(semantic_vector_ref(index, spec.dimensions)),
            });
            embed_queue.push((index, source));
        }
    }
    let texts = embed_queue
        .iter()
        .map(|(_, source)| source.content.clone())
        .collect::<Vec<_>>();

    if let Some(sink) = progress {
        sink.report(
            "embedding",
            "Embedding local semantic blocks",
            0,
            source_count,
            serde_json::json!({
                "documents": documents.len(),
                "blocks": blocks.len(),
                "reused": blocks.len().saturating_sub(texts.len()),
                "toEmbed": texts.len(),
                "modelId": spec.model_id,
                "dimensions": spec.dimensions,
                "batchSize": batch_size,
            }),
        )?;
    }
    let vectors = embed_texts_with_progress(app, &texts, "search_document", batch_size, progress)?;
    if vectors.len() != embed_queue.len() {
        return Err(format!(
            "fastembed returned {} vectors for {} semantic blocks",
            vectors.len(),
            embed_queue.len()
        ));
    }

    check_semantic_index_cancelled(progress)?;
    for ((index, _source), vector) in embed_queue.into_iter().zip(vectors) {
        if vector.len() != spec.dimensions {
            return Err(format!(
                "semantic vector for block {} has {} dimensions; omphalos requires {}",
                blocks[index].iri,
                vector.len(),
                spec.dimensions
            ));
        }
        blocks[index].vector = normalize_vector(vector);
        blocks[index].vector_ref = Some(semantic_vector_ref(index, spec.dimensions));
    }
    let entities = semantic_entities_from_blocks(&blocks, spec.dimensions);
    let document_count = documents.len();
    let indexed_at = timestamp();
    let index_path = semantic_index_path(&graph_dir);
    let index = SemanticIndexFile {
        manifest: SemanticIndexManifest {
            schema_version: SEMANTIC_INDEX_SCHEMA_VERSION,
            graph_id: graph_id.to_string(),
            provider_id: spec.provider_id.to_string(),
            model_id: spec.model_id.to_string(),
            dimensions: spec.dimensions,
            block_count: blocks.len(),
            document_count,
            indexed_at,
            index_path: display_path(&index_path),
        },
        blocks,
        entities,
    };
    if let Some(sink) = progress {
        let max_entity_cap = semantic_scaffold_max_entities();
        sink.report(
            "neighbors",
            "Building semantic scaffold",
            0,
            index.blocks.len(),
            serde_json::json!({
                "blocks": index.blocks.len(),
                "materializedNeighborLimit": SemanticScaffoldConfig::default().materialized_neighbor_limit,
                "maxEntityCap": max_entity_cap,
            }),
        )?;
    }
    let scaffold = build_semantic_scaffold(
        &index,
        SemanticScaffoldConfig::default(),
        display_path(&crate::semantic_index_paths::semantic_scaffold_path(
            &graph_dir,
        )),
    )?;
    if let Some(sink) = progress {
        sink.report(
            "clustering",
            "Clustering semantic scaffold",
            scaffold.entities.len(),
            scaffold.entities.len(),
            serde_json::json!({
                "entities": scaffold.entities.len(),
                "neighborEdges": scaffold.neighbor_edges.len(),
                "clusters": scaffold.clusters.len(),
            }),
        )?;
    }
    check_semantic_index_cancelled(progress)?;
    let store = crate::rdf_service::open_graph_store(&graph_dir)?;
    let wires = collect_wire_relations(&store)?;
    let relation_cfg = SemanticRelationConfig::default();
    let relation_specs = relation_profile_specs(&store, &wires, relation_cfg);
    let relation_texts = relation_specs
        .iter()
        .map(|spec| spec.verbalization.clone())
        .collect::<Vec<_>>();
    if let Some(sink) = progress {
        sink.report(
            "embedding",
            "Embedding semantic relation profiles",
            0,
            relation_texts.len(),
            serde_json::json!({
                "wireRelations": wires.len(),
                "relationProfiles": relation_texts.len(),
                "modelId": spec.model_id,
                "dimensions": spec.dimensions,
                "batchSize": batch_size,
            }),
        )?;
    }
    let relation_vectors = embed_texts_with_progress(
        app,
        &relation_texts,
        "search_relation",
        batch_size,
        progress,
    )?;
    check_semantic_index_cancelled(progress)?;
    let relations = build_semantic_relations(
        graph_id,
        spec.dimensions,
        &index,
        &scaffold,
        &wires,
        &relation_specs,
        relation_vectors,
        display_path(&crate::semantic_index_paths::semantic_relation_profiles_path(&graph_dir)),
        relation_cfg,
    )?;
    if let Some(sink) = progress {
        sink.report(
            "writing",
            "Writing semantic index, scaffold, and relations",
            scaffold.entities.len(),
            scaffold.entities.len(),
            serde_json::json!({
                "blocks": index.blocks.len(),
                "sourceEntities": scaffold.manifest.source_entity_count,
                "entities": scaffold.entities.len(),
                "entitySelectionMode": scaffold.manifest.entity_selection_mode,
                "maxEntityCap": scaffold.manifest.max_entity_cap,
                "neighborEdges": scaffold.neighbor_edges.len(),
                "clusters": scaffold.clusters.len(),
                "wireRelations": wires.len(),
                "relationProfiles": relations.profiles.len(),
                "relationInteractionEdges": relations.interaction_edges.len(),
                "relationNeighborEdges": relations.relation_neighbor_edges.len(),
            }),
        )?;
    }
    write_semantic_index(&graph_dir, &index)?;
    write_semantic_scaffold(&graph_dir, &scaffold)?;
    write_semantic_relations(&graph_dir, &relations)?;
    reconcile_semantic_projection(&store, graph_id, &scaffold, Some(&relations))?;
    touch_graph_updated_at(&graph_dir)?;
    semantic_index_status(app, &graph_dir, graph_id)
}

fn honor_flush_boundary(
    app: &AppHandle,
    graph_id: &str,
    flush_boundary: SemanticRefreshFlushBoundary,
) -> Result<(), String> {
    let result = crate::app_runtime::async_runtime::block_on(flush_graph_projection(app.clone(), graph_id));
    match (flush_boundary, result) {
        (_, Ok(())) => Ok(()),
        (SemanticRefreshFlushBoundary::BestEffort, Err(error)) => {
            log::warn!("best-effort semantic refresh CRDT flush failed for {graph_id}: {error}");
            Ok(())
        }
        (SemanticRefreshFlushBoundary::Required, Err(error)) => Err(format!(
            "required semantic refresh CRDT flush failed for {graph_id}: {error}"
        )),
    }
}

fn previous_blocks_by_iri(
    previous_index: Option<&SemanticIndexFile>,
) -> BTreeMap<String, &SemanticBlockEmbedding> {
    previous_index
        .into_iter()
        .flat_map(|index| index.blocks.iter())
        .filter_map(|block| {
            let iri = if block.iri.trim().is_empty() {
                fallback_block_iri(block)
            } else {
                block.iri.clone()
            };
            (!iri.trim().is_empty()).then_some((iri, block))
        })
        .collect()
}

fn fallback_block_iri(block: &SemanticBlockEmbedding) -> String {
    if block.block_type == "document" || block.block_id == "body" {
        crate::rdf::document_subject(&block.document_id)
    } else {
        format!(
            "{}#block-{}",
            crate::rdf::document_subject(&block.document_id),
            block.block_id
        )
    }
}

fn reusable_previous_block(
    source: &SemanticBlockSource,
    previous_by_iri: &BTreeMap<String, &SemanticBlockEmbedding>,
    dimensions: usize,
    index: usize,
) -> Option<SemanticBlockEmbedding> {
    let previous = previous_by_iri.get(&source.iri)?;
    if previous.content_hash != source.content_hash || previous.vector.len() != dimensions {
        return None;
    }
    let mut block = (*previous).clone();
    block.iri = source.iri.clone();
    block.kind = source.kind.clone();
    block.graph_id = source.graph_id.clone();
    block.document_id = source.document_id.clone();
    block.document_title = source.document_title.clone();
    block.block_id = source.block_id.clone();
    block.block_type = source.block_type.clone();
    block.content = source.content.clone();
    block.content_hash = source.content_hash.clone();
    block.order = source.order;
    block.vector_ref = Some(semantic_vector_ref(index, dimensions));
    Some(block)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::semantic_index::{semantic_content_hash, SemanticIndexManifest};

    fn source(iri: &str, content: &str) -> SemanticBlockSource {
        SemanticBlockSource {
            iri: iri.to_string(),
            kind: "block".to_string(),
            graph_id: "graph-a".to_string(),
            document_id: "doc-a".to_string(),
            document_title: "Doc A".to_string(),
            block_id: "a".to_string(),
            block_type: "paragraph".to_string(),
            content: content.to_string(),
            content_hash: semantic_content_hash(content),
            order: 1.0,
        }
    }

    fn previous_index(iri: &str, content: &str, dimensions: usize) -> SemanticIndexFile {
        SemanticIndexFile {
            manifest: SemanticIndexManifest {
                schema_version: SEMANTIC_INDEX_SCHEMA_VERSION,
                graph_id: "graph-a".to_string(),
                provider_id: "fastembed".to_string(),
                model_id: "model-a".to_string(),
                dimensions,
                block_count: 1,
                document_count: 1,
                indexed_at: "2026-01-01T00:00:00Z".to_string(),
                index_path: "blocks.json".to_string(),
            },
            blocks: vec![SemanticBlockEmbedding {
                iri: iri.to_string(),
                kind: "block".to_string(),
                graph_id: "graph-a".to_string(),
                document_id: "doc-a".to_string(),
                document_title: "Old Doc".to_string(),
                block_id: "old".to_string(),
                block_type: "paragraph".to_string(),
                content: content.to_string(),
                content_hash: semantic_content_hash(content),
                order: 0.0,
                vector: vec![1.0; dimensions],
                vector_ref: Some(semantic_vector_ref(0, dimensions)),
            }],
            entities: Vec::new(),
        }
    }

    #[test]
    fn reusable_previous_block_requires_same_iri_hash_and_dimensions() {
        let unchanged = source(
            "urn:mnemosyne:local:document:doc-a#block-a",
            "alpha beta gamma",
        );
        let previous = previous_index(&unchanged.iri, &unchanged.content, 3);
        let by_iri = previous_blocks_by_iri(Some(&previous));

        let reused = reusable_previous_block(&unchanged, &by_iri, 3, 7)
            .expect("unchanged source reuses vector");

        assert_eq!(reused.vector, vec![1.0, 1.0, 1.0]);
        assert_eq!(reused.document_title, "Doc A");
        assert_eq!(reused.vector_ref, Some(semantic_vector_ref(7, 3)));
        assert!(
            reusable_previous_block(&unchanged, &by_iri, 2, 0).is_none(),
            "dimension drift forces re-embed"
        );
        let changed = source(&unchanged.iri, "changed alpha beta");
        assert!(
            reusable_previous_block(&changed, &by_iri, 3, 0).is_none(),
            "content hash drift forces re-embed"
        );
    }

    #[test]
    fn semantic_refresh_stages_match_foundation_job_contract() {
        assert_eq!(
            SEMANTIC_REFRESH_STAGES,
            &[
                "validating",
                "collecting",
                "embedding",
                "neighbors",
                "clustering",
                "writing",
            ]
        );
    }
}

/// Real (no-mock) end-to-end coverage for the remote-embeddings decouple: a
/// real seeded graph + a real document written through the real CRDT engine
/// (mirroring `document_meaningful_object.rs`'s `harness_tests` setup),
/// against a real fake embeddings pool over a real TCP listener
/// (`semantic_model_remote::test_support`). NO MOCKS.
#[cfg(all(test, feature = "headless"))]
mod remote_integration_tests {
    use super::*;
    use crate::app_runtime::AppHandle;
    use crate::crdt_operation_types::EnqueueCrdtOperationInput;
    use crate::crdt_queue::enqueue_crdt_operation;
    use crate::graph_service::{create_graph_service, CreateGraphInput};
    use crate::semantic_model_remote::test_support::{
        embed_input_count, embed_vectors_response, remote_embeddings_test_serial, spawn_fake_pool,
    };
    use crate::semantic_model_remote::REMOTE_EMBEDDINGS_ENV;
    use crate::semantic_search_service::{semantic_search, SemanticSearchInput};
    use serde_json::json;
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};

    /// `GARDEN_PROFILE_DIR` is process-global; serialize against every other
    /// headless test in the binary that touches it, same convention as
    /// `document_meaningful_object.rs`'s `harness_tests`.
    fn profile_serial() -> &'static std::sync::Mutex<()> {
        crate::tauri_runtime::profile_env_serial()
    }

    fn temp_profile(name: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        std::env::temp_dir().join(format!("garden-semantic-remote-{name}-{nanos}"))
    }

    fn mock_app() -> AppHandle {
        crate::tauri_runtime::build_mock_app_for_tests(true)
    }

    fn seed_graph(app: &AppHandle, graph_id: &str) {
        create_graph_service(
            app,
            CreateGraphInput {
                graph_id: Some(graph_id.to_string()),
                title: "Remote Semantic Lab".to_string(),
                description: None,
                operation_id: None,
            },
        )
        .expect("create graph");
    }

    /// Write `markdown` into `doc_id` through the REAL CRDT engine — the same
    /// `document.write` op the spine/applier enqueue — so `read_graph_documents_cold`
    /// picks up a genuine persisted document, not a hand-built fixture.
    fn write_doc(app: &AppHandle, graph_id: &str, doc_id: &str, markdown: &str) {
        crate::app_runtime::async_runtime::block_on(enqueue_crdt_operation(
            app.clone(),
            EnqueueCrdtOperationInput {
                kind: "document.write".to_string(),
                graph_id: graph_id.to_string(),
                document_id: Some(doc_id.to_string()),
                payload: json!({
                    "documentId": doc_id,
                    "content": markdown,
                    "format": "markdown",
                    "title": "Doc",
                }),
            },
        ))
        .expect("document.write drains through the real CRDT engine");
    }

    #[test]
    fn refresh_skips_omphalos_dimension_recheck_when_remote_configured() {
        let _profile_serial = profile_serial().lock().unwrap_or_else(|p| p.into_inner());
        let _remote_serial = remote_embeddings_test_serial()
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        std::env::remove_var("SOPHIA_OMPHALOS");
        let profile = temp_profile("skips-omphalos-recheck");
        std::env::set_var("GARDEN_PROFILE_DIR", &profile);

        let endpoint = spawn_fake_pool(|_method, path, body| match path {
            "/embed" => (200, embed_vectors_response(embed_input_count(body), 8)),
            _ => (404, "not found".to_string()),
        });
        std::env::set_var(REMOTE_EMBEDDINGS_ENV, &endpoint);

        let app = mock_app();
        let graph_id = "remote-skips-omphalos-recheck";
        seed_graph(&app, graph_id);
        write_doc(
            &app,
            graph_id,
            "doc-a",
            "Alpha beta gamma delta content for the remote refresh regression guard.",
        );

        // If the redundant omphalos re-check at the top of this function
        // were NOT gated on remote_embeddings_endpoint(), this call would
        // fail immediately with an "omphalos constitution is absent" error —
        // there is no constitution.ttl anywhere under `profile`.
        let status = refresh_semantic_index_with_progress(
            &app,
            graph_id,
            crate::semantic_index_refresh_jobs::SemanticRefreshFlushBoundary::BestEffort,
            None,
        )
        .expect("refresh succeeds although no constitution exists, once remote is configured");
        assert!(status.block_count > 0);
        assert!(!profile.join("omphalos").exists());

        std::env::remove_var(REMOTE_EMBEDDINGS_ENV);
        let _ = std::fs::remove_dir_all(&profile);
    }

    #[test]
    fn semantic_search_and_reindex_succeed_on_a_cell_with_no_constitution_when_remote_pool_configured(
    ) {
        let _profile_serial = profile_serial().lock().unwrap_or_else(|p| p.into_inner());
        let _remote_serial = remote_embeddings_test_serial()
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        std::env::remove_var("SOPHIA_OMPHALOS");
        let profile = temp_profile("no-constitution");
        std::env::set_var("GARDEN_PROFILE_DIR", &profile);

        let endpoint = spawn_fake_pool(|_method, path, body| match path {
            "/embed" => (200, embed_vectors_response(embed_input_count(body), 32)),
            _ => (404, "not found".to_string()),
        });
        std::env::set_var(REMOTE_EMBEDDINGS_ENV, &endpoint);

        let app = mock_app();
        let graph_id = "remote-semantic-lab";
        seed_graph(&app, graph_id);
        write_doc(
            &app,
            graph_id,
            "doc-a",
            "The quick brown fox jumps over the lazy dog in the garden.",
        );

        assert!(
            !profile.join("omphalos").exists(),
            "no omphalos store should exist before refresh"
        );

        let status = refresh_semantic_index_with_progress(
            &app,
            graph_id,
            crate::semantic_index_refresh_jobs::SemanticRefreshFlushBoundary::BestEffort,
            None,
        )
        .expect("reindex succeeds on a cell with no constitution when a remote pool is configured");
        assert!(
            status.block_count > 0,
            "expected at least one embedded block"
        );

        assert!(
            !profile.join("omphalos").exists(),
            "remote reindex must never create a profile-local omphalos store"
        );

        let hits = semantic_search(
            app.clone(),
            SemanticSearchInput {
                graph_id: graph_id.to_string(),
                query: "fox".to_string(),
                limit: Some(5),
            },
        )
        .expect(
            "semantic search succeeds on a cell with no constitution when a remote pool is configured",
        );
        assert!(!hits.hits.is_empty(), "expected at least one search hit");

        std::env::remove_var(REMOTE_EMBEDDINGS_ENV);
        let _ = std::fs::remove_dir_all(&profile);
    }

    #[test]
    fn reindex_triggers_full_rebuild_when_pool_model_id_changes_between_runs() {
        let _profile_serial = profile_serial().lock().unwrap_or_else(|p| p.into_inner());
        let _remote_serial = remote_embeddings_test_serial()
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        std::env::remove_var("SOPHIA_OMPHALOS");
        let profile = temp_profile("model-swap");
        std::env::set_var("GARDEN_PROFILE_DIR", &profile);

        let endpoint_a = spawn_fake_pool(|_method, path, body| match path {
            "/embed" => (200, embed_vectors_response(embed_input_count(body), 16)),
            "/info" => (200, json!({ "model_id": "pool-model-a" }).to_string()),
            _ => (404, "not found".to_string()),
        });
        std::env::set_var(REMOTE_EMBEDDINGS_ENV, &endpoint_a);

        let app = mock_app();
        let graph_id = "remote-semantic-swap-lab";
        seed_graph(&app, graph_id);
        write_doc(
            &app,
            graph_id,
            "doc-a",
            "Alpha content about foxes and gardens for the model swap test.",
        );

        let first = refresh_semantic_index_with_progress(
            &app,
            graph_id,
            crate::semantic_index_refresh_jobs::SemanticRefreshFlushBoundary::BestEffort,
            None,
        )
        .expect("first reindex succeeds");
        assert_eq!(first.model_id, "pool-model-a");
        assert_eq!(first.dimensions, 16);

        let endpoint_b = spawn_fake_pool(|_method, path, body| match path {
            "/embed" => (200, embed_vectors_response(embed_input_count(body), 24)),
            "/info" => (200, json!({ "model_id": "pool-model-b" }).to_string()),
            _ => (404, "not found".to_string()),
        });
        std::env::set_var(REMOTE_EMBEDDINGS_ENV, &endpoint_b);

        let second = refresh_semantic_index_with_progress(
            &app,
            graph_id,
            crate::semantic_index_refresh_jobs::SemanticRefreshFlushBoundary::BestEffort,
            None,
        )
        .expect("second reindex succeeds against the new pool model");
        assert_eq!(second.model_id, "pool-model-b");
        assert_eq!(second.dimensions, 24);
        assert_eq!(
            second.block_count, first.block_count,
            "same document set: a full rebuild is not a document-count change"
        );

        std::env::remove_var(REMOTE_EMBEDDINGS_ENV);
        let _ = std::fs::remove_dir_all(&profile);
    }
}

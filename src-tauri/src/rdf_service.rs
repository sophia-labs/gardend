use crate::app_runtime::AppHandle;
#[cfg(test)]
use crate::document_service::{
    DocumentRecord, DocumentTreeSnapshot, TreeNodeAttributes, TreeNodeSnapshot,
};
#[cfg(test)]
use crate::rdf::{document_subject, format_rdf_triple};
use crate::{
    app_error::{AppError, AppResult},
    graph_service::touch_graph_updated_at,
    paths::existing_graph_dir,
    rdf_authority::{
        user_rdf_graph_iri, user_rdf_target_graph_iri, validate_sparql_update_authority,
    },
    rdf_mcp_inputs::{
        external_sparql_options_from_mcp_args, rdf_dump_input_from_mcp_args,
        rdf_load_input_from_mcp_args, sparql_input_from_mcp_args,
        sparql_update_input_from_mcp_args,
    },
    rdf_query_service::{
        classify_rdf_dataset_graph_targets, dump_rdf_from_store, execute_sparql_query_capped,
        execute_sparql_query_capped_with_control, execute_sparql_update,
        load_rdf_dataset_into_store, load_rdf_into_store, RdfDatasetTargetRefusal,
    },
    sparql_admission::{run_external_sparql_query, run_external_sparql_update},
};
use oxigraph::sparql::CancellationToken;
use serde::Deserialize;

pub(super) use crate::rdf_document_tree::document_tree_triples;
pub(super) use crate::rdf_query_service::{MutationResult, RdfDumpResult, SparqlQueryResult};
pub(super) use crate::rdf_record_materializer::{
    materialize_document_record, materialize_document_record_with_triples,
    materialize_graph_record, reconcile_graph_record,
};
pub(super) use crate::rdf_seed_service::{
    ensure_graph_store_seeded, ensure_graph_store_seeded_with_cancellation,
};
pub(super) use crate::rdf_store_service::open_graph_store;
pub(super) use crate::rdf_workspace_store_materializer::reconcile_workspace_snapshot;
pub(super) use crate::rdf_workspace_terms::{snapshot_array, wire_predicate_uri};

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct SparqlInput {
    pub(super) graph_id: String,
    pub(super) query: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct SparqlUpdateInput {
    pub(super) graph_id: String,
    pub(super) update: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct RdfLoadInput {
    pub(super) graph_id: String,
    pub(super) data: String,
    pub(super) format: String,
    pub(super) base_iri: Option<String>,
    pub(super) target_graph_iri: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct RdfDumpInput {
    pub(super) graph_id: String,
    pub(super) format: String,
    pub(super) source_graph_iri: Option<String>,
}

pub(super) async fn mcp_local_sparql_query(
    app: AppHandle,
    arguments: &serde_json::Value,
) -> AppResult<serde_json::Value> {
    let result = run_external_sparql_query(
        app,
        sparql_input_from_mcp_args(arguments),
        external_sparql_options_from_mcp_args(arguments),
    )
    .await?;
    serde_json::to_value(result)
        .map_err(|error| AppError::serialization(format!("serialize sparql result: {error}")))
}

pub(super) async fn mcp_local_sparql_update(
    app: AppHandle,
    arguments: &serde_json::Value,
) -> AppResult<serde_json::Value> {
    let options = external_sparql_options_from_mcp_args(arguments);
    if options.timeout_ms.is_some() {
        return Err(AppError::validation(
            "timeoutMs/timeout_ms is not accepted for SPARQL updates: Oxigraph 0.5.9 cannot cooperatively cancel every update operation",
        ));
    }
    if options.max_rows.is_some() {
        return Err(AppError::validation(
            "maxRows/max_rows applies only to SPARQL queries",
        ));
    }
    let result =
        run_external_sparql_update(app, sparql_update_input_from_mcp_args(arguments)).await?;
    serde_json::to_value(result).map_err(|error| {
        AppError::serialization(format!("serialize sparql update result: {error}"))
    })
}

pub(super) fn mcp_local_rdf_load(
    app: AppHandle,
    arguments: &serde_json::Value,
) -> AppResult<serde_json::Value> {
    let result = load_rdf_service(app, rdf_load_input_from_mcp_args(arguments))?;
    serde_json::to_value(result)
        .map_err(|error| AppError::serialization(format!("serialize rdf load result: {error}")))
}

pub(super) fn mcp_local_rdf_dump(
    app: AppHandle,
    arguments: &serde_json::Value,
) -> AppResult<serde_json::Value> {
    let result = dump_rdf_service(app, rdf_dump_input_from_mcp_args(arguments))?;
    serde_json::to_value(result)
        .map_err(|error| AppError::serialization(format!("serialize rdf dump result: {error}")))
}

#[cfg_attr(feature = "desktop", tauri::command)]
pub(super) fn run_sparql_query(
    app: AppHandle,
    input: SparqlInput,
) -> Result<SparqlQueryResult, String> {
    run_sparql_query_service(app, input).map_err(AppError::message)
}

pub(super) fn run_sparql_query_service(
    app: AppHandle,
    input: SparqlInput,
) -> AppResult<SparqlQueryResult> {
    let graph_dir = existing_graph_dir(&app, &input.graph_id).map_err(AppError::storage)?;
    ensure_graph_store_seeded(&graph_dir).map_err(AppError::rdf)?;
    let store = open_graph_store(&graph_dir).map_err(AppError::rdf)?;
    // EXTERNAL boundary: this is the user/agent-submitted `sparql_query`
    // (Tauri IPC, MCP tool, loopback route). Apply the row/triple ceiling here
    // — internal consumers call the uncapped `execute_sparql_query` directly.
    execute_sparql_query_capped(&store, &input.query).map_err(AppError::rdf)
}

/// Controlled external-boundary variant. The caller owns admission, deadline,
/// blocking-executor placement, and the graph lifecycle lease.
pub(super) fn run_sparql_query_service_controlled(
    app: AppHandle,
    input: SparqlInput,
    max_rows: usize,
    cancellation: CancellationToken,
) -> AppResult<SparqlQueryResult> {
    let graph_dir = existing_graph_dir(&app, &input.graph_id).map_err(AppError::storage)?;
    ensure_graph_store_seeded_with_cancellation(&graph_dir, &cancellation)
        .map_err(AppError::rdf)?;
    let store = open_graph_store(&graph_dir).map_err(AppError::rdf)?;
    execute_sparql_query_capped_with_control(&store, &input.query, max_rows, cancellation)
        .map_err(AppError::rdf)
}

#[cfg_attr(feature = "desktop", tauri::command)]
pub(super) fn run_sparql_update(
    app: AppHandle,
    input: SparqlUpdateInput,
) -> Result<MutationResult, String> {
    run_sparql_update_service(app, input).map_err(AppError::message)
}

pub(super) fn run_sparql_update_service(
    app: AppHandle,
    input: SparqlUpdateInput,
) -> AppResult<MutationResult> {
    let graph_dir = existing_graph_dir(&app, &input.graph_id).map_err(AppError::storage)?;
    ensure_graph_store_seeded(&graph_dir).map_err(AppError::rdf)?;
    // Default a graph-LESS `INSERT/DELETE DATA` into the graph's user:rdf named
    // graph instead of the store DEFAULT graph, so every "equivalent" write path
    // lands in the one graph every reader queries (`user_rdf_graph_iri`).
    let update = default_data_into_user_rdf_graph(&input.graph_id, &input.update);
    validate_sparql_update_authority(&input.graph_id, &update).map_err(AppError::validation)?;
    let store = open_graph_store(&graph_dir).map_err(AppError::rdf)?;
    let result = execute_sparql_update(&store, &update).map_err(AppError::rdf)?;
    touch_graph_updated_at(&graph_dir).map_err(AppError::storage)?;
    Ok(result)
}

/// Re-route a graph-LESS `INSERT DATA`/`DELETE DATA` update into the graph's
/// `user:rdf` named graph (`user_rdf_graph_iri`). Without this, bare `INSERT
/// DATA { ... }` lands in the store DEFAULT graph while every emporium reader
/// queries `GRAPH <{root}:user:rdf>`, so "equivalent" paths write provably
/// different physical graphs.
///
/// BOUNDARY: the update is rewritten ONLY when, AFTER any leading `PREFIX`/`BASE`
/// declarations, it is a single `INSERT DATA`/`DELETE DATA` block (trailing `}`,
/// mirroring `emporium::applier::graph_wrap`) that does NOT already name a graph.
/// A leading prefix prelude is preserved verbatim ahead of the rewritten verb —
/// workflow provenance emits `PREFIX … INSERT DATA { … }`, and the earlier
/// `^INSERT DATA` anchor missed it (dropping provenance into the DEFAULT graph).
/// Updates that already specify an explicit `GRAPH <…>`/`WITH <…>` clause, and
/// every `DELETE/INSERT … WHERE` / `LOAD` / multi-op form, pass through VERBATIM.
/// Targeting is detected by the `GRAPH`/`WITH` *keyword* (followed by an IRI/var),
/// so the substrings `graph`/`with` inside a literal or IRI no longer falsely
/// suppress the rewrite.
fn default_data_into_user_rdf_graph(graph_id: &str, update: &str) -> String {
    let trimmed = update.trim();
    // Split off any leading PREFIX/BASE declarations so a prefix-led
    // `PREFIX … INSERT DATA { … }` is still detected; the prelude is preserved.
    let prelude_re =
        regex::Regex::new(r"(?is)^((?:\s*(?:PREFIX\s+[^\s:]*:\s*<[^>]*>|BASE\s+<[^>]*>))*\s*)")
            .expect("prefix prelude regex is valid");
    let prelude_end = prelude_re.find(trimmed).map_or(0, |m| m.end());
    let prelude = &trimmed[..prelude_end];
    let rest = trimmed[prelude_end..].trim();

    // An explicit `GRAPH <…>`/`WITH <…>` clause means the update already targets a
    // graph — leave it alone. Keyword-anchored (not a bare substring) so `graph`/
    // `with` inside literals/IRIs don't trip it.
    let targeted_re = regex::Regex::new(r"(?is)\b(?:GRAPH\s*[<?]|WITH\s+<)")
        .expect("graph-target regex is valid");
    if targeted_re.is_match(rest) {
        return update.to_string();
    }

    let verb_re = regex::Regex::new(r"(?is)^(INSERT\s+DATA|DELETE\s+DATA)\s*\{")
        .expect("DATA verb regex is valid");
    let Some(captures) = verb_re.captures(rest) else {
        return update.to_string();
    };
    if !rest.ends_with('}') {
        return update.to_string();
    }
    let verb_word = captures.get(1).expect("verb capture present").as_str();
    let open = captures.get(0).expect("match present").end();
    let close = rest.rfind('}').expect("trailing brace present");
    let body = &rest[open..close];
    let user_rdf = user_rdf_graph_iri(graph_id);
    format!("{prelude}{verb_word} {{ GRAPH <{user_rdf}> {{\n{body}\n}} }}")
}

#[cfg_attr(feature = "desktop", tauri::command)]
pub(super) fn load_rdf(app: AppHandle, input: RdfLoadInput) -> Result<MutationResult, String> {
    load_rdf_service(app, input).map_err(AppError::message)
}

pub(super) fn load_rdf_service(app: AppHandle, input: RdfLoadInput) -> AppResult<MutationResult> {
    let graph_dir = existing_graph_dir(&app, &input.graph_id).map_err(AppError::storage)?;
    ensure_graph_store_seeded(&graph_dir).map_err(AppError::rdf)?;
    let store = open_graph_store(&graph_dir).map_err(AppError::rdf)?;
    let target_graph_iri =
        user_rdf_target_graph_iri(&input.graph_id, input.target_graph_iri.as_deref())
            .map_err(AppError::validation)?;
    let result = load_rdf_into_store(
        &store,
        &input.data,
        &input.format,
        input.base_iri.as_deref(),
        Some(target_graph_iri.as_str()),
    )
    .map_err(AppError::rdf)?;
    touch_graph_updated_at(&graph_dir).map_err(AppError::storage)?;
    Ok(result)
}

#[cfg_attr(feature = "desktop", tauri::command)]
pub(super) fn load_rdf_dataset(
    app: AppHandle,
    input: RdfLoadInput,
) -> Result<MutationResult, String> {
    load_rdf_dataset_service(app, input).map_err(AppError::message)
}

pub(super) fn load_rdf_dataset_service(
    app: AppHandle,
    input: RdfLoadInput,
) -> AppResult<MutationResult> {
    let graph_dir = existing_graph_dir(&app, &input.graph_id).map_err(AppError::storage)?;
    // A9 (§A.6/A.9): dataset formats (TriG/N-Quads/JSON-LD) carry their OWN
    // named-graph IRIs INLINE in `input.data`, unlike `load_rdf` whose single
    // target is an explicit argument — so, unlike `load_rdf_service`, there is
    // nothing to check until the payload is parsed. Pre-check EVERY named
    // graph the payload carries BEFORE `ensure_graph_store_seeded` — not just
    // before `open_graph_store`/`load_rdf_dataset_into_store` — so a request
    // that is ultimately refused never triggers `ensure_graph_store_seeded`'s
    // own store-materialization writes (graph record / workspace / document
    // re-seeding) as an unrelated side effect (review r1 finding). A
    // `Reserved` refusal maps to `AppError::validation` (HTTP 400) — the SAME
    // kind the SPARQL-update gate and `load_rdf`'s `user_rdf_target_graph_iri`
    // refusal already use; a `ParseFailure` (bad format/base IRI/syntax) maps
    // to `AppError::rdf` (HTTP 500), consistent with `load_rdf_into_store`'s
    // own parse-failure mapping.
    classify_rdf_dataset_graph_targets(
        &input.graph_id,
        &input.data,
        &input.format,
        input.base_iri.as_deref(),
    )
    .map_err(|refusal| match refusal {
        RdfDatasetTargetRefusal::Reserved(message) => AppError::validation(message),
        RdfDatasetTargetRefusal::ParseFailure(message) => AppError::rdf(message),
    })?;
    ensure_graph_store_seeded(&graph_dir).map_err(AppError::rdf)?;
    let store = open_graph_store(&graph_dir).map_err(AppError::rdf)?;
    let result = load_rdf_dataset_into_store(
        &store,
        &input.graph_id,
        &input.data,
        &input.format,
        input.base_iri.as_deref(),
    )
    .map_err(AppError::rdf)?;
    touch_graph_updated_at(&graph_dir).map_err(AppError::storage)?;
    Ok(result)
}

#[cfg_attr(feature = "desktop", tauri::command)]
pub(super) fn dump_rdf(app: AppHandle, input: RdfDumpInput) -> Result<RdfDumpResult, String> {
    dump_rdf_service(app, input).map_err(AppError::message)
}

pub(super) fn dump_rdf_service(app: AppHandle, input: RdfDumpInput) -> AppResult<RdfDumpResult> {
    let graph_dir = existing_graph_dir(&app, &input.graph_id).map_err(AppError::storage)?;
    ensure_graph_store_seeded(&graph_dir).map_err(AppError::rdf)?;
    let store = open_graph_store(&graph_dir).map_err(AppError::rdf)?;
    let default_graph_iri = user_rdf_graph_iri(&input.graph_id);
    dump_rdf_from_store(
        &store,
        &input.format,
        input.source_graph_iri.as_deref(),
        Some(default_graph_iri.as_str()),
    )
    .map_err(AppError::rdf)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime_config::{DOCUMENT_SCHEMA_VERSION, LOCAL_GRAPH_ORIGIN, LOCAL_PROVIDER_ID};

    #[test]
    fn document_tree_triples_capture_block_text_and_atom_content() {
        let document = DocumentRecord {
            document_id: "doc-a".to_string(),
            graph_id: "graph-a".to_string(),
            title: "Document A".to_string(),
            revision: 1,
            body: String::new(),
            origin: LOCAL_GRAPH_ORIGIN.to_string(),
            provider_id: LOCAL_PROVIDER_ID.to_string(),
            local_path: "/tmp/doc-a".to_string(),
            rdf_subject: document_subject("doc-a"),
            created_at: "1".to_string(),
            updated_at: "2".to_string(),
            capabilities: Vec::new(),
            schema_version: DOCUMENT_SCHEMA_VERSION,
            tiptap_xml: String::new(),
            tiptap_json: None,
            ydoc_update_base64: String::new(),
            ydoc_state_path: String::new(),
            tree: Some(DocumentTreeSnapshot {
                doc_id: "doc-a".to_string(),
                root: TreeNodeSnapshot {
                    kind: "element".to_string(),
                    tag_name: Some("doc".to_string()),
                    text_content: None,
                    attributes: TreeNodeAttributes::default(),
                    children: vec![
                        TreeNodeSnapshot {
                            kind: "element".to_string(),
                            tag_name: Some("paragraph".to_string()),
                            text_content: None,
                            attributes: TreeNodeAttributes {
                                block_id: Some("block-a".to_string()),
                                ..TreeNodeAttributes::default()
                            },
                            children: vec![TreeNodeSnapshot {
                                kind: "text".to_string(),
                                tag_name: None,
                                text_content: Some("Hello RDF".to_string()),
                                attributes: TreeNodeAttributes::default(),
                                children: Vec::new(),
                            }],
                        },
                        TreeNodeSnapshot {
                            kind: "element".to_string(),
                            tag_name: Some("image".to_string()),
                            text_content: None,
                            attributes: TreeNodeAttributes {
                                block_id: Some("image-a".to_string()),
                                src: Some("/image.png".to_string()),
                                alt: Some("An image".to_string()),
                                ..TreeNodeAttributes::default()
                            },
                            children: Vec::new(),
                        },
                    ],
                },
            }),
            blocks: Vec::new(),
            rdf_triple_count: 0,
            document_kind: None,
        };

        let rendered = document_tree_triples(&document)
            .iter()
            .map(format_rdf_triple)
            .collect::<Vec<_>>()
            .join("\n");

        assert!(rendered.contains("<urn:mnemosyne:local:document:doc-a#block-block-a>"));
        assert!(rendered.contains("<http://mnemosyne.dev/doc#textContent> \"Hello RDF\""));
        assert!(rendered.contains("<http://mnemosyne.dev/doc#imageSrc> \"/image.png\""));
        assert!(rendered.contains("<http://mnemosyne.dev/doc#altText> \"An image\""));
    }

    /// SERVICE-level (not primitive-level) proof: drives the real
    /// `#[cfg_attr(feature = "desktop", tauri::command)]`-backing `load_rdf_dataset_service` — the exact
    /// function the `load_rdf_dataset` command invokes — against a REAL
    /// on-disk graph (created via `create_graph_service`, opened via the
    /// real `existing_graph_dir`), with a REAL `RdfLoadInput`. Review r1
    /// findings this pins:
    ///   - a reserved-target `load_rdf_dataset` call is refused as
    ///     `AppError::validation` (HTTP 400), matching the SPARQL-update and
    ///     `load_rdf` refusal kind;
    ///   - the pre-check runs BEFORE `ensure_graph_store_seeded` — observed
    ///     directly via `rdf_seed_service`'s own reseed-storm test counter,
    ///     not inferred from source order: a refused request must leave the
    ///     seed's reseed count at 0, while an accepted request bumps it to 1;
    ///   - a non-reserved import is still accepted end-to-end and the quad
    ///     is genuinely queryable back out of the real store afterward.
    #[test]
    fn load_rdf_dataset_service_precheck_runs_before_seeding_and_refuses_reserved_targets() {
        let _serial = crate::tauri_runtime::profile_env_serial()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let profile = std::env::temp_dir().join(format!(
            "garden-load-rdf-dataset-service-{}",
            uuid::Uuid::new_v4()
        ));
        std::env::set_var("GARDEN_PROFILE_DIR", &profile);

        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let app = crate::tauri_runtime::build_mock_app_for_tests(true);
            let graph_id = "load-rdf-dataset-service-graph";
            crate::graph_service::create_graph_service(
                &app,
                crate::graph_service::CreateGraphInput {
                    title: "Load RDF Dataset Service Test".to_string(),
                    graph_id: Some(graph_id.to_string()),
                    description: None,
                    operation_id: None,
                },
            )
            .expect("create graph");

            let graph_dir = existing_graph_dir(&app, graph_id).expect("graph dir");
            let store_path = graph_dir.join("store.oxigraph");

            assert_eq!(
                crate::rdf_seed_service::reseed_count(&store_path),
                0,
                "no RDF request has run against this store yet"
            );

            let reserved_input = RdfLoadInput {
                graph_id: graph_id.to_string(),
                data: format!(
                    r#"<urn:s> <urn:p> "value" <urn:mnemosyne:local:graph:{graph_id}:projection:obs:raw> ."#
                ),
                format: "application/n-quads".to_string(),
                base_iri: None,
                target_graph_iri: None,
            };
            let error = load_rdf_dataset_service(app.clone(), reserved_input)
                .expect_err("reserved-target dataset import must be refused");
            assert_eq!(
                error.kind(),
                crate::app_error::AppErrorKind::Validation,
                "a reserved-target refusal must be AppError::validation (HTTP 400): {error}"
            );
            assert_eq!(
                crate::rdf_seed_service::reseed_count(&store_path),
                0,
                "a refused request must NEVER trigger ensure_graph_store_seeded's own writes"
            );

            // Positive control: the identical shape, retargeted at the
            // non-reserved :user:rdf graph, is accepted end-to-end — proving
            // the refusal above is reserved-ness-specific, AND that seeding
            // DOES run (exactly once) for a request that is actually let
            // through.
            let accepted_input = RdfLoadInput {
                graph_id: graph_id.to_string(),
                data: format!(
                    r#"<urn:s> <urn:p> "value" <urn:mnemosyne:local:graph:{graph_id}:user:rdf> ."#
                ),
                format: "application/n-quads".to_string(),
                base_iri: None,
                target_graph_iri: None,
            };
            let mutation = load_rdf_dataset_service(app.clone(), accepted_input)
                .expect("non-reserved dataset import must succeed");
            assert!(mutation.ok);
            // `quad_count` is the STORE TOTAL (`mutation_result`), not just
            // this call's delta — `ensure_graph_store_seeded` materializes
            // the graph record itself as RDF before our one quad lands, so
            // assert ">= 1" here and confirm the SPECIFIC quad landed via
            // the ASK read-back below, rather than an exact count that would
            // be coupled to how many triples graph-record materialization
            // happens to emit.
            assert!(mutation.quad_count >= 1);
            assert_eq!(
                crate::rdf_seed_service::reseed_count(&store_path),
                1,
                "an accepted request DOES run ensure_graph_store_seeded exactly once"
            );

            let query_result = run_sparql_query_service(
                app.clone(),
                SparqlInput {
                    graph_id: graph_id.to_string(),
                    query: format!(
                        "ASK {{ GRAPH <urn:mnemosyne:local:graph:{graph_id}:user:rdf> {{ <urn:s> <urn:p> ?o }} }}"
                    ),
                },
            )
            .expect("query the real store");
            assert_eq!(
                query_result.boolean,
                Some(true),
                "the accepted quad must genuinely be queryable back out of the real store"
            );
        }));

        std::env::remove_var("GARDEN_PROFILE_DIR");
        let _ = std::fs::remove_dir_all(&profile);
        if let Err(payload) = result {
            std::panic::resume_unwind(payload);
        }
    }

    #[test]
    fn graph_less_data_updates_default_into_user_rdf_graph() {
        // Bare INSERT/DELETE DATA → wrapped into the graph's user:rdf graph.
        let inserted =
            default_data_into_user_rdf_graph("graph-a", "INSERT DATA { <urn:s> <urn:p> <urn:o> }");
        assert!(inserted.contains("GRAPH <urn:mnemosyne:local:graph:graph-a:user:rdf>"));
        assert!(inserted.trim_start().starts_with("INSERT DATA"));
        assert!(inserted.contains("<urn:s> <urn:p> <urn:o>"));

        let deleted =
            default_data_into_user_rdf_graph("graph-a", "DELETE DATA { <urn:s> <urn:p> <urn:o> }");
        assert!(deleted.trim_start().starts_with("DELETE DATA"));
        assert!(deleted.contains("GRAPH <urn:mnemosyne:local:graph:graph-a:user:rdf>"));

        // The wrapped form still passes the authority gate (user:rdf is not reserved).
        assert!(validate_sparql_update_authority("graph-a", &inserted).is_ok());
    }

    #[test]
    fn updates_with_explicit_graph_or_where_pass_through_verbatim() {
        // Already names a GRAPH → untouched (no double wrap).
        let explicit = "INSERT DATA { GRAPH <urn:custom:g> { <urn:s> <urn:p> <urn:o> } }";
        assert_eq!(
            default_data_into_user_rdf_graph("graph-a", explicit),
            explicit
        );

        // DELETE/INSERT … WHERE is not a DATA block → untouched.
        let where_form = "DELETE { ?s ?p ?o } WHERE { ?s ?p ?o }";
        assert_eq!(
            default_data_into_user_rdf_graph("graph-a", where_form),
            where_form
        );

        // WITH-targeted update → untouched.
        let with_form = "WITH <urn:custom:g> DELETE { ?s ?p ?o } WHERE { ?s ?p ?o }";
        assert_eq!(
            default_data_into_user_rdf_graph("graph-a", with_form),
            with_form
        );
    }

    #[test]
    fn prefix_led_insert_data_is_wrapped_into_user_rdf() {
        // Workflow provenance emits `PREFIX … INSERT DATA { … }`. The prelude must
        // be preserved and the body wrapped into user:rdf — the earlier
        // `^INSERT DATA` anchor missed this and orphaned provenance in the DEFAULT
        // graph (verified live on the canary cell).
        let prov = "PREFIX wf: <http://mnemosyne.dev/workflow#>\nPREFIX prov: <http://www.w3.org/ns/prov#>\nINSERT DATA {\n<urn:run:1> a wf:Run ; wf:status \"completed\" .\n}";
        let wrapped = default_data_into_user_rdf_graph("graph-a", prov);
        assert!(
            wrapped.contains("PREFIX wf: <http://mnemosyne.dev/workflow#>"),
            "prelude preserved: {wrapped}"
        );
        assert!(
            wrapped.contains("GRAPH <urn:mnemosyne:local:graph:graph-a:user:rdf>"),
            "body wrapped into user:rdf: {wrapped}"
        );
        assert!(
            wrapped.contains("<urn:run:1> a wf:Run"),
            "triples preserved: {wrapped}"
        );
        assert!(validate_sparql_update_authority("graph-a", &wrapped).is_ok());

        // A literal containing the substring "graph" must NOT suppress the wrap.
        let literal = "INSERT DATA { <urn:s> <urn:p> \"a graph database\" }";
        let w2 = default_data_into_user_rdf_graph("graph-a", literal);
        assert!(
            w2.contains("GRAPH <urn:mnemosyne:local:graph:graph-a:user:rdf>"),
            "literal 'graph' must not block the wrap: {w2}"
        );
    }
}

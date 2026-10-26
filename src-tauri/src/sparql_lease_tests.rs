//! R1: an external SPARQL query takes the shared lifecycle lease, not the
//! hot-write lease, so it neither waits for a writer nor holds one up.
//! Real app state, real coordinator, real store; no stand-ins.
use std::time::Duration;

use serde_json::json;

use crate::crdt_engine::persistence_coordinator::GraphPersistenceCoordinator;
use crate::rdf_service::SparqlInput;
use crate::sparql_admission::{run_external_sparql_query, ExternalSparqlOptions};

const GRAPH: &str = "sparql-lease-graph";
const COUNT_ALL: &str = "SELECT (COUNT(*) AS ?n) WHERE { GRAPH ?g { ?s ?p ?o } }";
const DOCS: &str =
    "SELECT ?d WHERE { GRAPH ?g { ?d a <http://mnemosyne.dev/doc#TipTapDocument> } }";

fn run<F: std::future::Future>(future: F) -> F::Output {
    crate::app_runtime::async_runtime::block_on(future)
}

fn with_app(test: impl FnOnce(&crate::app_runtime::AppHandle)) {
    let _serial = crate::tauri_runtime::profile_env_serial()
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let previous = std::env::var_os("GARDEN_PROFILE_DIR");
    let profile = std::env::temp_dir().join(format!("garden-sparql-lease-{}", uuid::Uuid::new_v4()));
    std::env::set_var("GARDEN_PROFILE_DIR", &profile);
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let app = crate::tauri_runtime::build_mock_app_for_tests(true);
        crate::graph_service::create_graph_service(
            &app,
            crate::graph_service::CreateGraphInput {
                graph_id: Some(GRAPH.into()),
                title: "Lease".into(),
                description: None,
                operation_id: None,
            },
        )
        .unwrap();
        test(&app);
    }));
    match previous {
        Some(value) => std::env::set_var("GARDEN_PROFILE_DIR", value),
        None => std::env::remove_var("GARDEN_PROFILE_DIR"),
    }
    let _ = std::fs::remove_dir_all(&profile);
    if let Err(panic) = outcome {
        std::panic::resume_unwind(panic);
    }
}

async fn query(
    app: &crate::app_runtime::AppHandle,
    text: &str,
) -> crate::app_error::AppResult<crate::rdf_query_service::SparqlQueryResult> {
    run_external_sparql_query(
        app.clone(),
        SparqlInput {
            graph_id: GRAPH.to_string(),
            query: text.to_string(),
        },
        ExternalSparqlOptions {
            timeout_ms: Some(20_000),
            ..Default::default()
        },
    )
    .await
}

/// A writer holds the hot-write lease (every CRDT apply does). Before R1 the
/// query waited for it; now, with the seed current, it completes at once.
#[test]
fn a_query_does_not_wait_for_the_hot_write_holder() {
    with_app(|app| {
        run(async {
            // The first query may walk the seed, which needs the hot-write lease.
            query(app, COUNT_ALL).await.expect("warm-up query");
            let coordinator = app
                .try_state::<GraphPersistenceCoordinator>()
                .expect("coordinator");
            let writer = coordinator.acquire_hot_write(GRAPH).await.expect("writer");
            let answered = tokio::time::timeout(Duration::from_secs(5), query(app, COUNT_ALL))
                .await
                .expect("the query must not queue behind the hot-write holder")
                .expect("query ok");
            assert_eq!(answered.result_type, "solutions");
            drop(writer);
        });
    });
}

/// The lease a running query holds is shared: a writer takes the hot-write
/// lease while it is held, and the query's own lease is released at the end.
#[test]
fn a_writer_does_not_wait_for_a_running_query() {
    with_app(|app| {
        run(async {
            let coordinator = app
                .try_state::<GraphPersistenceCoordinator>()
                .expect("coordinator");
            // What a query holds for as long as it runs.
            let running_query = coordinator
                .acquire_lifecycle_shared(GRAPH)
                .await
                .expect("shared lease");
            let writer = tokio::time::timeout(
                Duration::from_secs(5),
                coordinator.acquire_hot_write(GRAPH),
            )
            .await
            .expect("a writer must not wait for a query's lease")
            .expect("hot-write lease");
            drop(writer);
            drop(running_query);
            // Deletion and restore still fence on the shared lease.
            let running_query = coordinator.acquire_lifecycle_shared(GRAPH).await.unwrap();
            let exclusive = tokio::time::timeout(
                Duration::from_millis(200),
                coordinator.acquire_lifecycle_exclusive(GRAPH),
            )
            .await;
            assert!(
                exclusive.is_err(),
                "the exclusive lifecycle lease still waits for a running query"
            );
            drop(running_query);
        });
    });
}

/// A document saved since the last query makes the seed stale: the shared
/// attempt writes nothing, the query escalates to the hot-write lease, walks,
/// and sees the saved document (read-after-write).
#[test]
fn a_stale_seed_escalates_and_the_query_sees_the_saved_document() {
    with_app(|app| {
        run(async {
            query(app, COUNT_ALL).await.expect("warm-up query");
            crate::mcp_workspace_crdt_service::mcp_local_create_document(
                app.clone(),
                &json!({"graphId": GRAPH, "documentId": "lease-doc", "title": "Lease doc"}),
            )
            .await
            .expect("create document");
            let answered = tokio::time::timeout(Duration::from_secs(20), query(app, DOCS))
                .await
                .expect("query finishes")
                .expect("query ok");
            assert!(
                !answered.rows.is_empty(),
                "the saved document is visible to the next query"
            );
        });
    });
}

//! A `source_pull` that starts after a workspace room broadcast carries that
//! write in its tree, and `source_push` and `source_rebuild` beside a write wait
//! for it (found by the self-host tree follow-ups of 2026-10-13).
//!
//! The self-host shell follows the workspace room: when the room announces a
//! change it pulls, and it draws its tree with SPARQL over the pulled RDF
//! projection (`projectionSnapshot`), not over the pulled Y.Doc. So a pull that
//! begins after an announcement must carry the announced write in the
//! projection, not only in `workspace.updateBase64`.
//!
//! - A sync client's frame (`Room::apply_client_update`, the workspace
//!   socket's path) is made durable and broadcast at once, while its tree
//!   projection is a `crdt.flush` that the room's debounce queues about 250 ms
//!   later and the executor then applies. A pull in between dumped the
//!   projection as it was before the frame. Red before the change: the pulled
//!   Y.Doc had the folder, the pulled tree did not.
//! - A queued write (MCP or REST, through the executor) holds the graph's
//!   hot-write lease from its broadcast through `persist_workspace`, and the
//!   pull dumped under the same lease, so its projection was never behind. But
//!   the pull's preparation (the RDF seed walk, the checkpoint) ran under the
//!   lifecycle lease only, beside the write. On `delete_document` the walk read
//!   the document the delete had just tombstoned, and the pull failed
//!   ("document tombstoned"): a follower's pull on a trash returned an error,
//!   not the trash. The create half passed before the change and is a guard;
//!   the delete half was red.
//!
//! `source_push` and `source_rebuild` (the second half of this file) had the
//! same preparation and the same replay; see the section comment there.
//!
//! A disposable profile; the real room registry, persistence coordinator,
//! queue, executor, MCP tool table and `mcp_local_source_pull`; the tree is read
//! from the pulled projection with the shell's own queries
//! (`apps/organism/src/cell/sidebar-documents.ts`). No mocks. Every wait is
//! bounded.
use crate::app_runtime::AppHandle;
use crate::crdt_engine::persistence_coordinator::GraphPersistenceCoordinator;
use crate::crdt_engine::rooms::{Room, RoomRegistry};
use crate::crdt_queue::CrdtOperationQueue;
use oxigraph::io::RdfFormat;
use oxigraph::model::Term;
use oxigraph::sparql::{QueryResults, SparqlEvaluator};
use oxigraph::store::Store;
use serde_json::{json, Value};
use std::sync::Arc;
use std::time::Duration;
use yrs::updates::decoder::Decode;
use yrs::{Any, Doc, Map, MapPrelim, Out, ReadTxn, StateVector, Transact, Update, WriteTxn};

const BOUND: Duration = Duration::from_secs(30);

/// One tree row as the shell's sidebar reads it.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Row {
    kind: &'static str,
    id: String,
    label: String,
    parent: Option<String>,
}

fn literal(term: Option<&Term>) -> Option<String> {
    match term? {
        Term::Literal(literal) => Some(literal.value().to_string()),
        Term::NamedNode(node) => Some(node.as_str().to_string()),
        other => Some(other.to_string()),
    }
}

/// The local id at the end of a workspace subject IRI (`...:folder:{id}`).
fn bare(iri: &str) -> String {
    iri.rsplit(':').next().unwrap_or(iri).to_string()
}

/// Load the pull's projection into a fresh store, as the page's local copy
/// does, and read the tree with the shell's folder and document queries.
fn tree(graph_id: &str, pulled: &Value) -> Vec<Row> {
    let store = Store::new().expect("in-memory store");
    store
        .load_from_slice(
            RdfFormat::NQuads,
            pulled["projectionSnapshot"]["data"]
                .as_str()
                .expect("projection snapshot")
                .as_bytes(),
        )
        .expect("load the pulled projection");
    let ws = crate::rdf_authority::workspace_projection_graph_iri(graph_id);
    let folders = [
        "PREFIX doc: <http://mnemosyne.dev/doc#>",
        "PREFIX nfo: <http://www.semanticdesktop.org/ontologies/2007/03/22/nfo#>",
        "SELECT ?folder (COALESCE(?name, \"Untitled Folder\") AS ?label) ?parent ?order ?section",
        &format!("WHERE {{ GRAPH <{ws}> {{"),
        "  ?folder a doc:Folder .",
        "  OPTIONAL { ?folder nfo:fileName ?name }",
        "  OPTIONAL { ?folder nfo:belongsToContainer ?parent }",
        "  OPTIONAL { ?folder doc:order ?order }",
        "  OPTIONAL { ?folder doc:section ?section }",
        "} }",
        "ORDER BY ?order LCASE(STR(?label))",
    ]
    .join("\n");
    let documents = [
        "PREFIX doc: <http://mnemosyne.dev/doc#>",
        "PREFIX dcterms: <http://purl.org/dc/terms/>",
        "PREFIX nfo: <http://www.semanticdesktop.org/ontologies/2007/03/22/nfo#>",
        "SELECT ?doc (COALESCE(?dctitle, ?doctitle, \"Untitled\") AS ?label) ?parent ?order",
        &format!("WHERE {{ GRAPH <{ws}> {{"),
        "  ?doc a doc:TipTapDocument .",
        "  OPTIONAL { ?doc dcterms:title ?dctitle }",
        "  OPTIONAL { ?doc doc:title ?doctitle }",
        "  OPTIONAL { ?doc nfo:belongsToContainer ?parent }",
        "  OPTIONAL { ?doc doc:order ?order }",
        "} }",
        "ORDER BY ?order LCASE(STR(?label))",
    ]
    .join("\n");
    let mut rows = Vec::new();
    for (kind, subject, query) in [
        ("folder", "folder", folders),
        ("document", "doc", documents),
    ] {
        let results = SparqlEvaluator::new()
            .parse_query(&query)
            .expect("parse the shell's tree query")
            .on_store(&store)
            .execute()
            .expect("run the shell's tree query");
        let QueryResults::Solutions(solutions) = results else {
            panic!("tree query returned no solutions");
        };
        for solution in solutions {
            let solution = solution.expect("tree solution");
            rows.push(Row {
                kind,
                id: bare(&literal(solution.get(subject)).expect("subject")),
                label: literal(solution.get("label")).unwrap_or_default(),
                parent: literal(solution.get("parent")).map(|parent| bare(&parent)),
            });
        }
    }
    rows
}

fn shows(rows: &[Row], kind: &str, id: &str) -> Option<Row> {
    rows.iter()
        .find(|row| row.kind == kind && row.id == id)
        .cloned()
}

/// Whether the pulled workspace Y.Doc (not its projection) has `id` in `map`.
fn pulled_ydoc_has(pulled: &Value, map: &str, id: &str) -> bool {
    let bytes = base64::Engine::decode(
        &base64::engine::general_purpose::STANDARD,
        pulled["workspace"]["updateBase64"]
            .as_str()
            .expect("pulled workspace"),
    )
    .expect("decode pulled workspace");
    let doc = Doc::new();
    doc.transact_mut()
        .apply_update(Update::decode_v1(&bytes).expect("decode update"))
        .expect("apply pulled workspace");
    let txn = doc.transact();
    txn.get_map(map)
        .map(|entries| matches!(entries.get(&txn, id), Some(Out::YMap(_))))
        .unwrap_or(false)
}

async fn pull(app: &AppHandle, graph_id: &str) -> Value {
    tokio::time::timeout(
        BOUND,
        crate::source_sync::mcp_local_source_pull(app.clone(), &json!({ "graphId": graph_id })),
    )
    .await
    .expect("source_pull answered within the bound")
    .expect("source_pull")
}

async fn heard(announcements: &mut tokio::sync::broadcast::Receiver<Vec<u8>>, what: &str) {
    tokio::time::timeout(BOUND, announcements.recv())
        .await
        .unwrap_or_else(|_| panic!("the room did not announce {what} within the bound"))
        .unwrap_or_else(|error| panic!("room broadcast for {what}: {error}"));
}

async fn until_clean(room: &Room, what: &str) {
    tokio::time::timeout(BOUND, async {
        while room.needs_projection_flush() {
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("the workspace projection did not settle after {what}"));
}

/// The workspace room as the socket route opens it: hydrate, install the
/// debounced projection flush, schedule the connect-time repair.
async fn open_room_as_the_socket_does(app: &AppHandle, graph_id: &str) -> Arc<Room> {
    let graph_dir = crate::paths::existing_graph_dir(app, graph_id).expect("graph dir");
    let room = app
        .state::<RoomRegistry>()
        .get_or_create(
            &format!("workspace:{graph_id}"),
            crate::ydoc_paths::workspace_ydoc_state_path(&graph_dir),
        )
        .await
        .expect("workspace room");
    room.configure_projection_flush(graph_id.to_string(), None)
        .expect("configure the room's projection flush");
    room.schedule_projection_flush(app.clone());
    until_clean(&room, "the connect-time repair").await;
    room
}

/// A collaborator's frame: a new folder, and a document filed in it, written
/// straight into the room's maps (Garden's filesystem store writes this way).
fn collaborator_frame(folder_id: &str, folder: &str, document_id: &str, title: &str) -> Vec<u8> {
    let doc = Doc::new();
    {
        let mut txn = doc.transact_mut();
        let folders = txn.get_or_insert_map("folders");
        let entry = folders.insert(&mut txn, folder_id, MapPrelim::default());
        entry.insert(&mut txn, "name", folder);
        entry.insert(&mut txn, "section", "documents");
        entry.insert(&mut txn, "parentId", Any::Null);
        entry.insert(&mut txn, "order", 1.0);
        let documents = txn.get_or_insert_map("documents");
        let entry = documents.insert(&mut txn, document_id, MapPrelim::default());
        entry.insert(&mut txn, "title", title);
        entry.insert(&mut txn, "parentId", folder_id);
        entry.insert(&mut txn, "section", "documents");
        entry.insert(&mut txn, "order", 1.0);
        entry.insert(&mut txn, "createdAt", 1.0);
        entry.insert(&mut txn, "updatedAt", 1.0);
        entry.insert(&mut txn, "readOnly", false);
    }
    let txn = doc.transact();
    txn.encode_state_as_update_v1(&StateVector::default())
}

fn with_disposable_profile(
    label: &str,
    test: impl FnOnce(&std::path::Path) + std::panic::UnwindSafe,
) {
    let _serial = crate::tauri_runtime::profile_env_serial()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let previous = std::env::var_os("GARDEN_PROFILE_DIR");
    let profile = std::env::temp_dir().join(format!("garden-{label}-{}", uuid::Uuid::new_v4()));
    std::env::set_var("GARDEN_PROFILE_DIR", &profile);
    let profile_for_test = profile.clone();
    let outcome = std::panic::catch_unwind(move || test(&profile_for_test));
    match previous {
        Some(previous) => std::env::set_var("GARDEN_PROFILE_DIR", previous),
        None => std::env::remove_var("GARDEN_PROFILE_DIR"),
    }
    let _ = std::fs::remove_dir_all(&profile);
    if let Err(panic) = outcome {
        std::panic::resume_unwind(panic);
    }
}

fn create_graph(app: &AppHandle, graph_id: &str) {
    crate::graph_service::create_graph_service(
        app,
        crate::graph_service::CreateGraphInput {
            title: "Pull after flush".to_string(),
            graph_id: Some(graph_id.to_string()),
            description: None,
            operation_id: None,
        },
    )
    .expect("create graph");
}

#[test]
fn a_pull_on_a_sync_clients_broadcast_carries_that_write_in_its_tree() {
    with_disposable_profile("pull-after-flush-socket", |_| {
        crate::app_runtime::async_runtime::block_on(async {
            let graph_id = "pull-after-flush-socket";
            let (folder_id, folder) = ("followed-folder", "Followed folder");
            let (document_id, title) = ("followed-note", "Followed note");
            let app = crate::tauri_runtime::build_mock_app_for_tests(true);
            create_graph(&app, graph_id);

            // The page activates its local copy, then holds the room open.
            let activation = pull(&app, graph_id).await;
            let room = open_room_as_the_socket_does(&app, graph_id).await;
            assert!(shows(&tree(graph_id, &activation), "folder", folder_id).is_none());

            // A pull of a clean workspace touches no projection: the graph's
            // content revision (graphMetadata.updatedAt) does not move.
            let clean = pull(&app, graph_id).await;
            let again = pull(&app, graph_id).await;
            assert_eq!(
                clean["graphMetadata"]["updatedAt"], again["graphMetadata"]["updatedAt"],
                "a pull of a clean workspace must not flush or touch the graph"
            );

            // The executor is busy (gardend applies queued work one operation
            // at a time; a write elsewhere in the cell takes 1-2.5 s), so the
            // room's debounced `crdt.flush` cannot land while the page pulls.
            let queue = app.state::<CrdtOperationQueue>();
            let busy = queue.lock_drain().await;

            // The collaborator's frame, applied as the socket loop applies one.
            let mut announcements = room.subscribe_updates_for_test();
            {
                let coordinator = app.state::<GraphPersistenceCoordinator>();
                let lease = coordinator
                    .acquire_hot_write(graph_id)
                    .await
                    .expect("socket frame lease");
                lease.declare_rdf_read_only();
                assert!(room
                    .apply_client_update(&collaborator_frame(folder_id, folder, document_id, title))
                    .await
                    .expect("apply the collaborator's frame"));
                room.schedule_projection_flush(app.clone());
            }

            // The page hears the announcement and pulls.
            heard(&mut announcements, "the collaborator's frame").await;
            assert!(
                room.needs_projection_flush(),
                "premise: the room's queued projection flush has not landed yet"
            );
            let pulled = pull(&app, graph_id).await;
            let rows = tree(graph_id, &pulled);
            assert!(
                pulled_ydoc_has(&pulled, "folders", folder_id),
                "the pulled workspace Y.Doc carries the frame"
            );
            assert_eq!(
                shows(&rows, "folder", folder_id),
                Some(Row {
                    kind: "folder",
                    id: folder_id.into(),
                    label: folder.into(),
                    parent: None,
                }),
                "a pull that began after the room announced a folder must show it in its tree \
                 (the pulled Y.Doc has it; the pulled projection must too). tree: {rows:?}"
            );
            assert_eq!(
                shows(&rows, "document", document_id),
                Some(Row {
                    kind: "document",
                    id: document_id.into(),
                    label: title.into(),
                    parent: Some(folder_id.into()),
                }),
                "the document filed in it in the same frame. tree: {rows:?}"
            );
            assert_ne!(
                pulled["graphMetadata"]["updatedAt"], again["graphMetadata"]["updatedAt"],
                "the pull completed the write's ordinary projection tail (content revision moved)"
            );

            // The executor frees up; its queued flush finds nothing left to do,
            // and a later pull agrees with the first.
            drop(busy);
            until_clean(&room, "the queued flush").await;
            let later = pull(&app, graph_id).await;
            assert_eq!(tree(graph_id, &later), rows);
        });
    });
}

#[test]
fn a_pull_on_an_mcp_writes_broadcast_carries_that_write_in_its_tree() {
    with_disposable_profile("pull-after-flush-mcp", |profile| {
        crate::app_runtime::async_runtime::block_on(async {
            let graph_id = "pull-after-flush-mcp";
            let app = crate::tauri_runtime::build_mock_app_for_tests(true);
            create_graph(&app, graph_id);
            let jobs = Arc::new(
                crate::local_jobs::LocalJobRegistry::new(profile.join("jobs")).expect("jobs"),
            );
            let context = crate::mcp_dispatch_registry::McpCallCtx {
                app: app.clone(),
                jobs: &jobs,
            };
            let call = |tool: &'static str, args: Value| {
                let context = &context;
                async move {
                    let entry =
                        crate::mcp_dispatch_registry::lookup(tool).expect("tool in the table");
                    tokio::time::timeout(BOUND, (entry.handler)(context, &args))
                        .await
                        .unwrap_or_else(|_| panic!("{tool} did not answer within the bound"))
                        .unwrap_or_else(|error| panic!("{tool}: {error}"))
                }
            };

            let activation = pull(&app, graph_id).await;
            let room = open_room_as_the_socket_does(&app, graph_id).await;
            let (document_id, title) = ("mcp-followed", "Created through MCP");
            assert!(shows(&tree(graph_id, &activation), "document", document_id).is_none());

            // create_document: the page pulls on the first announcement, while
            // the MCP call is still in the executor.
            let mut announcements = room.subscribe_updates_for_test();
            let (_, pulled) = tokio::join!(
                call(
                    "create_document",
                    json!({ "graphId": graph_id, "documentId": document_id, "title": title }),
                ),
                async {
                    heard(&mut announcements, "create_document").await;
                    pull(&app, graph_id).await
                }
            );
            let rows = tree(graph_id, &pulled);
            assert_eq!(
                shows(&rows, "document", document_id).map(|row| row.label),
                Some(title.to_string()),
                "a pull on create_document's announcement shows the document. tree: {rows:?}"
            );

            // delete_document: the same, for the row leaving the tree. The pull
            // starts while the delete's tail is still running.
            let mut announcements = room.subscribe_updates_for_test();
            let (_, pulled) = tokio::join!(
                call(
                    "delete_document",
                    json!({ "graphId": graph_id, "documentId": document_id }),
                ),
                async {
                    heard(&mut announcements, "delete_document").await;
                    pull(&app, graph_id).await
                }
            );
            let rows = tree(graph_id, &pulled);
            assert!(
                !pulled_ydoc_has(&pulled, "documents", document_id),
                "the pulled Y.Doc no longer has the document"
            );
            assert!(
                shows(&rows, "document", document_id).is_none(),
                "a pull on delete_document's announcement no longer shows it. tree: {rows:?}"
            );
            until_clean(&room, "the MCP writes").await;
        });
    });
}

// ── source_push and source_rebuild ──────────────────────────────────────────
//
// Both prepared the same way the pull did (the seed walk and the checkpoint
// under the lifecycle lease only), so a queued delete running beside them
// failed them ("document tombstoned", or a manifest gone). Both replay the projections with
// `rebuild_source_projections`, which rebuilds the workspace face from the live
// room and marks the room persisted without the ordinary tail: a socket frame
// still waiting for its debounced flush lost its title sweep (push), and a
// rebuild measured `projectionBefore` without it and reported drift (rebuild).

/// Rounds of "a folder of documents, deleted hard with its contents, the caller
/// started on the delete's broadcast". The cascade removes the documents one at
/// a time, all under the graph's hot-write lease (a single document's delete
/// leaves only a few milliseconds between its tombstone and its directory
/// going). A caller whose seed walk listed them before and reads them after
/// failed: on a tombstone, or on a manifest gone since the listing. Each
/// document has a long body, so the walk's reads spread across the cascade.
const DELETE_ROUNDS: usize = 2;
const DOCUMENTS_PER_FOLDER: usize = 8;
const PARAGRAPHS_PER_DOCUMENT: usize = 200;

async fn mcp(
    context: &crate::mcp_dispatch_registry::McpCallCtx<'_>,
    tool: &'static str,
    args: Value,
) -> Value {
    let entry = crate::mcp_dispatch_registry::lookup(tool).expect("tool in the table");
    tokio::time::timeout(BOUND, (entry.handler)(context, &args))
        .await
        .unwrap_or_else(|_| panic!("{tool} did not answer within the bound"))
        .unwrap_or_else(|error| panic!("{tool}: {error}"))
}

/// Start `caller` on the room's first broadcast of an MCP folder delete (hard,
/// cascade), in rounds, and return each round's outcome.
async fn beside_deletes<F, Fut>(
    app: &AppHandle,
    profile: &std::path::Path,
    graph_id: &str,
    room: &Room,
    caller: F,
) -> Vec<Result<Value, String>>
where
    F: Fn(usize) -> Fut,
    Fut: std::future::Future<Output = crate::app_error::AppResult<Value>>,
{
    let jobs =
        Arc::new(crate::local_jobs::LocalJobRegistry::new(profile.join("jobs")).expect("jobs"));
    let context = crate::mcp_dispatch_registry::McpCallCtx {
        app: app.clone(),
        jobs: &jobs,
    };
    let body = (0..PARAGRAPHS_PER_DOCUMENT)
        .map(|index| format!("Paragraph {index} of a document the cascade deletes."))
        .collect::<Vec<_>>()
        .join("\n\n");
    let mut outcomes = Vec::new();
    for round in 0..DELETE_ROUNDS {
        let folder_id = format!("doomed-{round}");
        mcp(
            &context,
            "create_folder",
            json!({ "graphId": graph_id, "folderId": folder_id, "name": format!("Doomed {round}") }),
        )
        .await;
        for index in 0..DOCUMENTS_PER_FOLDER {
            let document_id = format!("doomed-{round}-{index}");
            mcp(
                &context,
                "create_document",
                json!({ "graphId": graph_id, "documentId": document_id, "title": format!("Doomed {round}.{index}"), "parentId": folder_id }),
            )
            .await;
            mcp(
                &context,
                "write_document",
                json!({ "graphId": graph_id, "documentId": document_id, "content": body, "format": "markdown" }),
            )
            .await;
        }
        let mut announcements = room.subscribe_updates_for_test();
        let (_, outcome) = tokio::join!(
            mcp(
                &context,
                "delete",
                json!({ "graphId": graph_id, "type": "folder", "folderId": folder_id, "cascade": true, "hard": true }),
            ),
            async {
                heard(&mut announcements, "the folder delete").await;
                tokio::time::timeout(BOUND, caller(round))
                    .await
                    .expect("the caller answered within the bound")
            }
        );
        outcomes.push(outcome.map_err(|error| error.to_string()));
    }
    outcomes
}

fn incarnation_of(pulled: &Value) -> String {
    pulled["graphIncarnation"]
        .as_str()
        .expect("graph incarnation")
        .to_string()
}

#[test]
fn a_push_started_beside_a_delete_waits_for_it() {
    with_disposable_profile("push-beside-delete", |profile| {
        crate::app_runtime::async_runtime::block_on(async {
            let graph_id = "push-beside-delete";
            let app = crate::tauri_runtime::build_mock_app_for_tests(true);
            create_graph(&app, graph_id);
            let incarnation = incarnation_of(&pull(&app, graph_id).await);
            let room = open_room_as_the_socket_does(&app, graph_id).await;
            let outcomes = beside_deletes(&app, profile, graph_id, &room, |round| {
                let app = app.clone();
                let args = json!({
                    "graphId": graph_id,
                    "graphIncarnation": incarnation,
                    "operations": [{
                        "kind": "graphMetadata",
                        "operationId": format!("meta-beside-delete-{round}"),
                        "title": format!("Pushed beside delete {round}"),
                    }],
                });
                async move { crate::source_sync::mcp_local_source_push(app, &args).await }
            })
            .await;
            let failed = outcomes
                .iter()
                .enumerate()
                .filter_map(|(round, outcome)| outcome.as_ref().err().map(|error| (round, error)))
                .collect::<Vec<_>>();
            assert!(
                failed.is_empty(),
                "a push started on a folder delete's announcement failed in {} of {DELETE_ROUNDS} rounds: {failed:?}",
                failed.len()
            );
            for outcome in &outcomes {
                assert_eq!(outcome.as_ref().unwrap()["ok"], true, "{outcome:?}");
            }
            until_clean(&room, "the deletes").await;
        });
    });
}

#[test]
fn a_rebuild_started_beside_a_delete_waits_for_it() {
    with_disposable_profile("rebuild-beside-delete", |profile| {
        crate::app_runtime::async_runtime::block_on(async {
            let graph_id = "rebuild-beside-delete";
            let app = crate::tauri_runtime::build_mock_app_for_tests(true);
            create_graph(&app, graph_id);
            let incarnation = incarnation_of(&pull(&app, graph_id).await);
            let room = open_room_as_the_socket_does(&app, graph_id).await;
            let outcomes = beside_deletes(&app, profile, graph_id, &room, |_| {
                let app = app.clone();
                let args = json!({ "graphId": graph_id, "graphIncarnation": incarnation });
                async move { crate::source_sync::mcp_local_source_rebuild(app, &args).await }
            })
            .await;
            let failed = outcomes
                .iter()
                .enumerate()
                .filter_map(|(round, outcome)| outcome.as_ref().err().map(|error| (round, error)))
                .collect::<Vec<_>>();
            assert!(
                failed.is_empty(),
                "a rebuild started on a folder delete's announcement failed in {} of {DELETE_ROUNDS} rounds: {failed:?}",
                failed.len()
            );
            until_clean(&room, "the deletes").await;
        });
    });
}

/// A collaborator's frame that renames an existing document: the room's state
/// is the client's, and the frame is the client's own increment over it.
async fn rename_frame(room: &Room, document_id: &str, title: &str) -> Vec<u8> {
    let client = Doc::new();
    client
        .transact_mut()
        .apply_update(Update::decode_v1(&room.encode_state_for_test().await).expect("room state"))
        .expect("apply room state");
    let before = client.transact().state_vector();
    {
        let mut txn = client.transact_mut();
        let documents = txn.get_or_insert_map("documents");
        let Some(Out::YMap(entry)) = documents.get(&txn, document_id) else {
            panic!("the room has no entry for {document_id}");
        };
        entry.insert(&mut txn, "title", title);
    }
    let txn = client.transact();
    txn.encode_state_as_update_v1(&before)
}

/// Apply a collaborator's frame as the socket loop applies one.
async fn apply_as_the_socket_does(app: &AppHandle, graph_id: &str, room: &Arc<Room>, frame: &[u8]) {
    let coordinator = app.state::<GraphPersistenceCoordinator>();
    let lease = coordinator
        .acquire_hot_write(graph_id)
        .await
        .expect("socket frame lease");
    lease.declare_rdf_read_only();
    assert!(room
        .apply_client_update(frame)
        .await
        .expect("apply the collaborator's frame"));
    room.schedule_projection_flush(app.clone());
}

fn record_title(app: &AppHandle, graph_id: &str, document_id: &str) -> String {
    let graph_dir = crate::paths::existing_graph_dir(app, graph_id).expect("graph dir");
    crate::document_service::read_graph_documents_cold(&graph_dir)
        .expect("document records")
        .into_iter()
        .find(|record| record.document_id == document_id)
        .map(|record| record.title)
        .unwrap_or_else(|| panic!("no record for {document_id}"))
}

/// One call of a tool from gardend's MCP table, as `/mcp` dispatches it.
async fn mcp_once(
    app: &AppHandle,
    profile: &std::path::Path,
    tool: &'static str,
    args: Value,
) -> Value {
    let jobs =
        Arc::new(crate::local_jobs::LocalJobRegistry::new(profile.join("jobs")).expect("jobs"));
    let context = crate::mcp_dispatch_registry::McpCallCtx {
        app: app.clone(),
        jobs: &jobs,
    };
    mcp(&context, tool, args).await
}

#[test]
fn a_push_while_a_collaborators_rename_waits_for_its_flush_keeps_the_rename() {
    with_disposable_profile("push-beside-rename", |profile| {
        crate::app_runtime::async_runtime::block_on(async {
            let graph_id = "push-beside-rename";
            let document_id = "renamed-by-a-collaborator";
            let app = crate::tauri_runtime::build_mock_app_for_tests(true);
            create_graph(&app, graph_id);
            let incarnation = incarnation_of(&pull(&app, graph_id).await);
            let room = open_room_as_the_socket_does(&app, graph_id).await;
            mcp_once(
                &app,
                profile,
                "create_document",
                json!({ "graphId": graph_id, "documentId": document_id, "title": "Before the rename" }),
            )
            .await;
            until_clean(&room, "the create").await;
            assert_eq!(
                record_title(&app, graph_id, document_id),
                "Before the rename"
            );

            // The executor is busy, so the frame's debounced flush waits while
            // the push lands (as in the pull's socket test).
            let queue = app.state::<CrdtOperationQueue>();
            let busy = queue.lock_drain().await;
            let frame = rename_frame(&room, document_id, "After the rename").await;
            apply_as_the_socket_does(&app, graph_id, &room, &frame).await;
            assert!(
                room.needs_projection_flush(),
                "premise: the rename's flush is pending"
            );
            let pushed = tokio::time::timeout(
                BOUND,
                crate::source_sync::mcp_local_source_push(
                    app.clone(),
                    &json!({
                        "graphId": graph_id,
                        "graphIncarnation": incarnation,
                        "operations": [{
                            "kind": "graphMetadata",
                            "operationId": "meta-beside-rename",
                            "title": "Pushed beside a rename",
                        }],
                    }),
                ),
            )
            .await
            .expect("the push answered within the bound")
            .expect("source_push");
            assert_eq!(pushed["ok"], true, "{pushed}");

            // The executor frees up. An explicit flush queues behind the
            // rename's debounced one, so once it answers that one has run too;
            // the rename must have reached the document's record by then.
            drop(busy);
            tokio::time::sleep(Duration::from_millis(400)).await; // past the 250 ms debounce
            mcp_once(&app, profile, "flush_crdt", json!({ "graphId": graph_id })).await;
            until_clean(&room, "the queued flush").await;
            assert_eq!(
                record_title(&app, graph_id, document_id),
                "After the rename",
                "a socket rename pending its flush when a push landed never reached the document's record"
            );
        });
    });
}

#[test]
fn a_rebuild_while_a_collaborators_frame_waits_for_its_flush_reports_no_drift() {
    with_disposable_profile("rebuild-beside-frame", |_| {
        crate::app_runtime::async_runtime::block_on(async {
            let graph_id = "rebuild-beside-frame";
            let app = crate::tauri_runtime::build_mock_app_for_tests(true);
            create_graph(&app, graph_id);
            let incarnation = incarnation_of(&pull(&app, graph_id).await);
            let room = open_room_as_the_socket_does(&app, graph_id).await;
            let rebuild = || {
                let app = app.clone();
                let args = json!({ "graphId": graph_id, "graphIncarnation": incarnation });
                async move { crate::source_sync::mcp_local_source_rebuild(app, &args).await }
            };
            // A fresh graph's first replay writes faces the cold path never
            // wrote (the empty workspace's, the Y.Doc projections), so it reports
            // a difference whatever the room holds. The second is the baseline.
            tokio::time::timeout(BOUND, rebuild())
                .await
                .expect("bounded")
                .expect("source_rebuild");
            let clean = tokio::time::timeout(BOUND, rebuild())
                .await
                .expect("bounded")
                .expect("source_rebuild");
            assert_eq!(
                clean["ok"], true,
                "premise: a rebuild of a clean graph reports no drift: {clean}"
            );

            let queue = app.state::<CrdtOperationQueue>();
            let busy = queue.lock_drain().await;
            apply_as_the_socket_does(
                &app,
                graph_id,
                &room,
                &collaborator_frame("drift-folder", "Drift folder", "drift-note", "Drift note"),
            )
            .await;
            assert!(
                room.needs_projection_flush(),
                "premise: the frame's flush is pending"
            );
            let rebuilt = tokio::time::timeout(BOUND, rebuild())
                .await
                .expect("bounded")
                .expect("source_rebuild");
            assert_eq!(
                rebuilt["projectionSetEqual"], true,
                "a rebuild with a frame pending its flush must not report drift: before {} after {}",
                rebuilt["projectionBefore"], rebuilt["projectionAfter"]
            );
            assert_eq!(rebuilt["ok"], true, "{rebuilt}");
            drop(busy);
            until_clean(&room, "the queued flush").await;
        });
    });
}

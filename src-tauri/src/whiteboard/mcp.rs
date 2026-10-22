//! The three whiteboard MCP tools. Handlers here are argument plumbing over the
//! pure core (`scene`, `outline`) and the Y.Doc adapters (`ops`):
//!
//! - `whiteboard_create {graphId, title?, parentId?}` creates a document of kind
//!   `whiteboard` with its `meta` root and returns `{documentId, title}`.
//! - `whiteboard_read {graphId, documentId, format?}` reads the live room (or the
//!   stored Y.Doc) as an outline, or as the raw elements.
//! - `whiteboard_apply {graphId, documentId, rationale?, moves}` applies the moves
//!   in order in ONE Y.Doc transaction on the live room, so every open browser sees
//!   them at once, then persists the record. A malformed move refuses the whole
//!   call before anything is written.

use super::ops::{self, revision_token};
use super::outline;
use super::scene::{apply_moves, bounds, unmark_wires, Ctx, ObjectInfo, Rng, Scene};
use super::WHITEBOARD_KIND;
use crate::app_runtime::AppHandle;
use crate::crdt_engine::rooms::{Room, RoomRegistry};
use crate::mcp_utils::{mcp_arg_string, mcp_required_document_id, mcp_required_graph_id};
use crate::scene_wire::{SceneWireDraft, SceneWireEnd};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::sync::Arc;
#[cfg(feature = "desktop")]
use tauri::Manager;

const MAX_MOVES: usize = 200;

/// One apply at a time per process: the compute (read) and the write are two
/// steps on the room, and two applies must not both compute from one baseline.
static APPLY_SERIAL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

struct Board {
    room: Arc<Room>,
    title: String,
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn actor_name() -> String {
    match crate::request_actor::current_request_actor() {
        Some(actor) => actor.via.unwrap_or(actor.user_id),
        None => "tool".to_string(),
    }
}

async fn workspace_title(app: &AppHandle, graph_id: &str, document_id: &str) -> Option<String> {
    let graph_dir = crate::graph_paths::existing_graph_dir(app, graph_id).ok()?;
    let ws = crate::crdt_engine::workspace_ops::workspace_room(app, graph_id, &graph_dir)
        .await
        .ok()?;
    let id = document_id.to_string();
    ws.with_doc(move |doc| crate::crdt_engine::workspace_ops::document_title_in_workspace(doc, &id))
        .await
}

/// A typed object as the board snapshots it: its title and class label, read through the same code
/// `emporium_describe` runs. An IRI the graph knows nothing about is refused (a card of nothing would point nowhere).
fn describe_object(app: &AppHandle, graph_id: &str, iri: &str) -> Result<ObjectInfo, String> {
    let missing = || format!("object '{iri}' is not in this graph");
    let described = crate::emporium::describe::mcp_local_emporium_describe(
        app.clone(),
        &json!({ "graphId": graph_id, "iri": iri }),
    )
    .map_err(|error| {
        if error.to_string().contains("is not an IRI") {
            format!("object '{iri}' is not an absolute IRI")
        } else {
            missing()
        }
    })?;
    if described.get("found").and_then(Value::as_bool) != Some(true) {
        return Err(missing());
    }
    let label = described
        .get("title")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|title| !title.is_empty())
        .map(str::to_string)
        // An object with no title says its own name: the last segment of its IRI.
        .unwrap_or_else(|| {
            iri.rsplit(['#', '/', ':'])
                .find(|part| !part.is_empty())
                .unwrap_or(iri)
                .to_string()
        });
    let class_label = described
        .get("class")
        .and_then(|class| class.get("label").or_else(|| class.get("name")))
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    Ok(ObjectInfo {
        label,
        class_label,
        graph_id: graph_id.to_string(),
    })
}

/// One end of a stored wire as the apply result reports it.
fn end_json(end: &SceneWireEnd) -> Value {
    let mut out = serde_json::Map::new();
    if let Some(id) = &end.document_id {
        out.insert("documentId".into(), json!(id));
    }
    if let Some(id) = &end.block_id {
        out.insert("blockId".into(), json!(id));
    }
    if let Some(iri) = &end.iri {
        out.insert("iri".into(), json!(iri));
    }
    Value::Object(out)
}

fn wires_json(wires: &[SceneWireDraft]) -> Value {
    Value::Array(
        wires
            .iter()
            .map(|wire| {
                json!({
                    "arrowId": wire.scene.element_id,
                    "wireId": wire.wire_id,
                    "predicate": wire.predicate,
                    "source": end_json(&wire.source),
                    "target": end_json(&wire.target),
                })
            })
            .collect(),
    )
}

/// The board's room, after checking the document is a whiteboard.
async fn open_board(app: &AppHandle, graph_id: &str, document_id: &str) -> Result<Board, String> {
    let kind =
        crate::crdt_engine::document_ops::resolve_document_kind(app, graph_id, document_id).await?;
    if kind.as_deref() != Some(WHITEBOARD_KIND) {
        return Err(format!(
            "document '{document_id}' is not a whiteboard (documentKind: {kind:?}); \
             make one with whiteboard_create"
        ));
    }
    let graph_dir = crate::graph_paths::existing_graph_dir(app, graph_id)?;
    let registry = app.state::<RoomRegistry>();
    let room = registry
        .get_or_create(
            &format!("doc:{graph_id}:{document_id}"),
            crate::ydoc_paths::document_ydoc_state_path(&graph_dir, document_id),
        )
        .await?;
    let title = workspace_title(app, graph_id, document_id)
        .await
        .unwrap_or_else(|| "Untitled whiteboard".to_string());
    Ok(Board { room, title })
}

pub(crate) async fn mcp_local_whiteboard_create(
    app: AppHandle,
    arguments: &Value,
) -> Result<Value, String> {
    let graph_id = mcp_required_graph_id(arguments)?;
    let title = mcp_arg_string(arguments, &["title"])
        .map(|t| t.trim().to_string())
        .filter(|t| !t.is_empty())
        .unwrap_or_else(|| "Untitled whiteboard".to_string());
    let mut create = json!({
        "graphId": graph_id,
        "title": title,
        "documentKind": WHITEBOARD_KIND,
    });
    if let Some(parent) = mcp_arg_string(arguments, &["parentId", "parent_id"]) {
        create["parentId"] = json!(parent);
    }
    let created =
        crate::mcp_workspace_crdt_service::mcp_local_create_document(app.clone(), &create).await?;
    let document_id = created
        .get("documentId")
        .and_then(Value::as_str)
        .ok_or("create_document did not say which document it made")?
        .to_string();
    let board = open_board(&app, &graph_id, &document_id).await?;
    board
        .room
        .update_doc(|_doc, txn| {
            ops::ensure_meta(txn);
            Ok(())
        })
        .await?;
    crate::crdt_engine::document_ops::persist_room_document(
        &app,
        &graph_id,
        &document_id,
        &title,
        &board.room,
        &format!("op-wb-create-{}", uuid::Uuid::new_v4().simple()),
    )
    .await?;
    Ok(json!({ "graphId": graph_id, "documentId": document_id, "title": title }))
}

pub(crate) async fn mcp_local_whiteboard_read(
    app: AppHandle,
    arguments: &Value,
) -> Result<Value, String> {
    let graph_id = mcp_required_graph_id(arguments)?;
    let document_id = mcp_required_document_id(arguments)?;
    let format = mcp_arg_string(arguments, &["format"]).unwrap_or_else(|| "outline".to_string());
    if !matches!(format.as_str(), "outline" | "elements") {
        return Err("format is outline or elements".into());
    }
    let board = open_board(&app, &graph_id, &document_id).await?;
    let (scene, revision) = board
        .room
        .with_doc(|doc| (ops::scene_of_doc(doc), revision_token(doc)))
        .await;
    Ok(read_result(
        &graph_id,
        &document_id,
        &board.title,
        &scene,
        &revision,
        &format,
    ))
}

fn read_result(
    graph_id: &str,
    document_id: &str,
    title: &str,
    scene: &Scene,
    revision: &str,
    format: &str,
) -> Value {
    let live = scene.live().count();
    let deleted = scene.elements.len() - live;
    if format == "elements" {
        return json!({
            "graphId": graph_id, "documentId": document_id, "title": title,
            "elements": scene.elements.values().collect::<Vec<_>>(),
            "elementCount": live, "deletedCount": deleted, "revision": revision,
        });
    }
    let shapes = outline::shapes(scene);
    let arrows = outline::arrows(scene);
    let bounds = bounds(scene).map(|b| {
        json!({
            "x": b.x.round() as i64, "y": b.y.round() as i64,
            "w": b.w.round() as i64, "h": b.h.round() as i64,
        })
    });
    json!({
        "graphId": graph_id, "documentId": document_id, "title": title,
        "shapes": shapes.iter().map(outline::shape_json).collect::<Vec<_>>(),
        "arrows": arrows.iter().map(outline::arrow_json).collect::<Vec<_>>(),
        "markdown": outline::outline_markdown(title, scene),
        "bounds": bounds,
        "elementCount": live, "deletedCount": deleted, "revision": revision,
    })
}

pub(crate) async fn mcp_local_whiteboard_apply(
    app: AppHandle,
    arguments: &Value,
) -> Result<Value, String> {
    let graph_id = mcp_required_graph_id(arguments)?;
    let document_id = mcp_required_document_id(arguments)?;
    let moves = arguments
        .get("moves")
        .and_then(Value::as_array)
        .filter(|m| !m.is_empty())
        .ok_or("moves is required: a non-empty list of moves")?;
    if moves.len() > MAX_MOVES {
        return Err(format!("at most {MAX_MOVES} moves in one call"));
    }
    let rationale = mcp_arg_string(arguments, &["rationale"]);
    let board = open_board(&app, &graph_id, &document_id).await?;

    // Titles of the documents a link or card move names, and the typed objects they name by IRI. Both are resolved
    // before anything is written: a missing target refuses the whole call.
    let mut titles = BTreeMap::new();
    let mut objects: BTreeMap<String, ObjectInfo> = BTreeMap::new();
    for (index, mv) in moves.iter().enumerate() {
        if !matches!(
            mv.get("op").and_then(Value::as_str),
            Some("link") | Some("card")
        ) {
            continue;
        }
        if let Some(doc) = mv.get("documentId").and_then(Value::as_str) {
            if let Some(title) = workspace_title(&app, &graph_id, doc).await {
                titles.insert(doc.to_string(), title);
            }
        }
        if let Some(iri) = mv.get("iri").and_then(Value::as_str) {
            if !objects.contains_key(iri) {
                let info = describe_object(&app, &graph_id, iri)
                    .map_err(|error| format!("move {}: {error}", index + 1))?;
                objects.insert(iri.to_string(), info);
            }
        }
    }

    let _serial = APPLY_SERIAL.lock().await;
    let actor = actor_name();
    let now = now_ms();
    let seed = (uuid::Uuid::new_v4().as_u128() as u64) ^ now;
    let computed = board
        .room
        .with_doc(|doc| {
            let mut scene = ops::scene_of_doc(doc);
            let mut ctx = Ctx {
                actor: &actor,
                now,
                rng: Rng(seed),
                graph_id: &graph_id,
                document_id: &document_id,
                titles: &titles,
                objects: &objects,
                wires: Vec::new(),
            };
            apply_moves(&mut scene, moves, &mut ctx).map(|created| (scene, created))
        })
        .await?;
    let (mut scene, mut created) = computed;
    let mut wires: Vec<SceneWireDraft> = Vec::new();
    if !scene.dirty.is_empty() {
        crate::restore_guard::require_no_active_restore(&app, &graph_id)?;
        let lease = match app
            .try_state::<crate::crdt_engine::persistence_coordinator::GraphPersistenceCoordinator>(
        ) {
            Some(coordinator) => Some(coordinator.acquire_hot_write(&graph_id).await?),
            None => None,
        };
        // The pass above only validated. A person may have drawn since, so the
        // moves run again on the state inside the write transaction (same seed,
        // same ids) and only that result is written; otherwise the board would
        // overwrite their edit with the baseline's copy of the element. A refusal
        // here is returned as a value: an Err from the closure evicts the room.
        let fresh = board
            .room
            .update_doc(|_doc, txn| {
                let mut live = ops::read_scene(&*txn);
                let mut ctx = Ctx {
                    actor: &actor,
                    now,
                    rng: Rng(seed),
                    graph_id: &graph_id,
                    document_id: &document_id,
                    titles: &titles,
                    objects: &objects,
                    wires: Vec::new(),
                };
                let applied = apply_moves(&mut live, moves, &mut ctx);
                let drafted = std::mem::take(&mut ctx.wires);
                Ok(applied.map(|made| {
                    ops::write_dirty(txn, &live);
                    (live, made, drafted)
                }))
            })
            .await?;
        drop(lease);
        (scene, created, wires) = fresh?;
        // The wires the `wire` moves decided on are stored after the scene is written. When they cannot be, the
        // arrows must not stay marked as wires that do not exist: unmark them in a second write, persist, and say so.
        if !wires.is_empty() {
            if let Err(error) =
                crate::scene_wire::create_scene_wires(&app, &graph_id, &wires).await
            {
                let arrows: Vec<String> = wires
                    .iter()
                    .map(|wire| wire.scene.element_id.clone())
                    .collect();
                let lease = match app
                    .try_state::<crate::crdt_engine::persistence_coordinator::GraphPersistenceCoordinator>()
                {
                    Some(coordinator) => Some(coordinator.acquire_hot_write(&graph_id).await?),
                    None => None,
                };
                board
                    .room
                    .update_doc(|_doc, txn| {
                        let mut live = ops::read_scene(&*txn);
                        let mut ctx = Ctx {
                            actor: &actor,
                            now,
                            rng: Rng(seed),
                            graph_id: &graph_id,
                            document_id: &document_id,
                            titles: &titles,
                            objects: &objects,
                            wires: Vec::new(),
                        };
                        unmark_wires(&mut live, &mut ctx, &arrows);
                        ops::write_dirty(txn, &live);
                        Ok(())
                    })
                    .await?;
                drop(lease);
                crate::crdt_engine::document_ops::persist_room_document(
                    &app,
                    &graph_id,
                    &document_id,
                    &board.title,
                    &board.room,
                    &format!("op-wb-unwire-{}", uuid::Uuid::new_v4().simple()),
                )
                .await?;
                return Err(format!("wire: {error}"));
            }
        }
        crate::crdt_engine::document_ops::persist_room_document(
            &app,
            &graph_id,
            &document_id,
            &board.title,
            &board.room,
            &format!("op-wb-apply-{}", uuid::Uuid::new_v4().simple()),
        )
        .await?;
    }
    let (after, revision) = board
        .room
        .with_doc(|doc| (ops::scene_of_doc(doc), revision_token(doc)))
        .await;
    let mut result = read_result(
        &graph_id,
        &document_id,
        &board.title,
        &after,
        &revision,
        "outline",
    );
    result["created"] = Value::Object(
        created
            .into_iter()
            .map(|(name, id)| (name, json!(id)))
            .collect(),
    );
    result["changed"] = json!(scene.dirty.len());
    result["wires"] = wires_json(&wires);
    if let Some(rationale) = rationale {
        result["rationale"] = json!(rationale);
    }
    Ok(result)
}

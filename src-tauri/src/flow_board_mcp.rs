//! Unit G4 — the two Mithras Flow board MCP tools:
//!
//! - `flow_seed_board { graphId, json, replace? }` → enqueues the `flow.seed`
//!   CRDT op (crdt_engine/flow_ops.rs), waits for the flush, and returns the
//!   seed stats (`documentId`, per-table `rows`, `subjects`, `extras`,
//!   `derive`).
//! - `flow_export_board { graphId, restorePointId? }` → canonical form-C
//!   export with the `sophia` sidecar key. Without a restore point the LIVE
//!   room is read under `Room::with_doc` (one consistency point — GEOM-13: a
//!   torn read is impossible by construction); with one, that restore point's
//!   persisted board bytes are decoded into a scratch Doc and the id travels
//!   in the file (FLOW-GARDEN-P1).
//!
//! Handlers here are thin: argument plumbing over the crdt_engine cores
//! (`flow_ops::apply_classified` via the queue for the seed,
//! `flow_ops::export_board` for the export). Declared in all four sources of
//! truth: `mcp_tool_catalog.json`, `mcp_dispatch_registry.rs`,
//! `parity/local-loopback-surface.json`, `cell_graph_boundary_policy.json`
//! (both graph-scoped; `graphId` exposed in each schema).

use crate::app_runtime::AppHandle;
use crate::{
    crdt_projection_flush::flush_graph_projection,
    crdt_queue::{enqueue_crdt_operation, EnqueueCrdtOperationInput},
    flow_board::BOARD_DOCUMENT_ID,
    mcp_utils::{mcp_arg_string, mcp_required_graph_id},
};
use serde_json::Value;

pub(super) async fn mcp_local_flow_seed_board(
    app: AppHandle,
    arguments: &Value,
) -> Result<Value, String> {
    let graph_id = mcp_required_graph_id(arguments)?;
    let json_text = arguments
        .get("json")
        .and_then(Value::as_str)
        .ok_or_else(|| "json is required (the Flow board file content, as a string)".to_string())?
        .to_string();
    let replace = arguments
        .get("replace")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let value = enqueue_crdt_operation(
        app.clone(),
        EnqueueCrdtOperationInput {
            kind: "flow.seed".to_string(),
            graph_id: graph_id.clone(),
            document_id: Some(BOARD_DOCUMENT_ID.to_string()),
            payload: serde_json::json!({ "json": json_text, "replace": replace }),
        },
    )
    .await?;
    flush_graph_projection(app, &graph_id).await?;
    Ok(value)
}

pub(super) async fn mcp_local_flow_export_board(
    app: AppHandle,
    arguments: &Value,
) -> Result<Value, String> {
    let graph_id = mcp_required_graph_id(arguments)?;
    let restore_point_id = mcp_arg_string(arguments, &["restorePointId", "restore_point_id"]);
    crate::crdt_engine::flow_ops::export_board(&app, &graph_id, restore_point_id.as_deref()).await
}

// ───────────────────────────────────────────────────────────────────────────
// Unit G4 dispatch-level tests. Written RED-FIRST: every call resolves the
// tool through the runtime dispatch table (`mcp_dispatch_registry::lookup`) —
// the exact path a live `tools/call` takes — so this file compiled and FAILED
// before the handlers/registrations above existed (red.txt in the unit's log
// dir), and passed unchanged after (green.txt). Pattern:
// `mcp_dispatch_registry::headless_dispatch_tests` + `flow_board_flush_tests`.
// ───────────────────────────────────────────────────────────────────────────
#[cfg(all(test, feature = "headless"))]
mod tests {
    use crate::flow_board::{board_export_json, board_from_json};
    use crate::graph_service::{create_graph_service, CreateGraphInput};
    use crate::local_jobs::LocalJobRegistry;
    use crate::mcp_dispatch_registry::{lookup, McpCallCtx};
    use serde_json::{json, Value};
    use std::panic::UnwindSafe;
    use std::path::{Path, PathBuf};
    use std::sync::Arc;
    use uuid::Uuid;

    const GOLDEN_BOARD_JSON: &str = include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/flow/golden-board.json"
    ));

    fn with_profile(prefix: &str, test: impl FnOnce() + UnwindSafe) {
        let _serial = crate::tauri_runtime::profile_env_serial()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let profile: PathBuf = std::env::temp_dir().join(format!("{prefix}-{}", Uuid::new_v4()));
        std::env::set_var("GARDEN_PROFILE_DIR", &profile);
        let result = std::panic::catch_unwind(test);
        std::env::remove_var("GARDEN_PROFILE_DIR");
        let _ = std::fs::remove_dir_all(&profile);
        if let Err(payload) = result {
            std::panic::resume_unwind(payload);
        }
    }

    /// Dispatch through the REAL registry entry — the same
    /// `(entry.handler)(ctx, args)` path `mcp_tool_dispatch.rs` runs for a
    /// live `tools/call`. RED before G4: `lookup` returns `None`.
    fn call(ctx: &McpCallCtx<'_>, name: &str, args: Value) -> Result<Value, String> {
        let entry = lookup(name)
            .unwrap_or_else(|| panic!("dispatch entry for {name} (G4 tool not registered)"));
        crate::app_runtime::async_runtime::block_on((entry.handler)(ctx, &args))
            .map_err(|error| error.to_string())
    }

    /// The reference canonical export (G2's fixture round-trip, incl. the
    /// FLOW-GARDEN-P2-amended derive) — what the seeded room must export.
    fn reference_export_json() -> String {
        board_export_json(
            &board_from_json(GOLDEN_BOARD_JSON).expect("golden fixture parses"),
            None,
        )
        .json
    }

    /// `reference` with the `sophia` sidecar spliced in before the final `}` —
    /// the exact byte sequence a cell export must produce (§E: the sidecar is
    /// appended last, after extras).
    fn with_sidecar(reference: &str, restore_point_id: Option<&str>, graph_id: &str) -> String {
        let rp = match restore_point_id {
            Some(id) => format!("\"{id}\""),
            None => "null".to_string(),
        };
        format!(
            "{}{}{}",
            &reference[..reference.len() - 1],
            format!(
                ",\"sophia\":{{\"restorePointId\":{rp},\"graphId\":\"{graph_id}\",\"documentId\":\"flow-board\"}}"
            ),
            "}"
        )
    }

    fn blank_system_extents(v: &mut Value) {
        if let Some(systems) = v.get_mut("systems").and_then(Value::as_array_mut) {
            for row in systems {
                if let Some(obj) = row.as_object_mut() {
                    obj.insert("width".to_string(), Value::Null);
                    obj.insert("height".to_string(), Value::Null);
                }
            }
        }
    }

    fn setup(graph_id: &str, jobs_dir: &Path) -> (crate::app_runtime::AppHandle, Arc<LocalJobRegistry>) {
        let app = crate::tauri_runtime::build_mock_app_for_tests(true);
        create_graph_service(
            &app,
            CreateGraphInput {
                title: format!("G4 {graph_id}"),
                graph_id: Some(graph_id.to_string()),
                description: None,
                operation_id: None,
            },
        )
        .expect("create graph");
        let jobs = Arc::new(
            LocalJobRegistry::new(jobs_dir.to_path_buf()).expect("create job registry"),
        );
        (app, jobs)
    }

    /// The whole G4 arc through dispatch: seed the golden board, export live
    /// (byte-exact against the G2 reference export + the sidecar), capture a
    /// restore point, replace the live board, and export FROM the restore
    /// point — pinned bytes, id travelling in the file (FLOW-GARDEN-P1).
    ///
    /// Byte-exactness, documented (the brief's "document which is byte-exact"):
    /// - live export minus the sidecar (textual strip) == the reference
    ///   canonical export — BYTE-EXACT;
    /// - vs the raw golden fixture it is NOT byte-exact: `systems[*].width/
    ///   height` are derive-corrected (G2's reported deviation — golden's own
    ///   `sys-uav` stored 420x280 fails containment and P2-amended emits
    ///   480x360); equal everywhere else, asserted by parsing both, deleting
    ///   `sophia`, blanking system extents, and comparing.
    #[test]
    fn seed_then_export_live_and_from_restore_point_through_dispatch() {
        with_profile("garden-g4-flow-mcp", || {
            let jobs_dir =
                std::env::temp_dir().join(format!("garden-g4-flow-mcp-jobs-{}", Uuid::new_v4()));
            let graph_id = "g4-flow-mcp";
            let (app, jobs) = setup(graph_id, &jobs_dir);
            let ctx = McpCallCtx {
                app: app.clone(),
                jobs: &jobs,
            };

            // Export before any seed: refused (no flow board declared).
            let err = call(&ctx, "flow_export_board", json!({ "graphId": graph_id }))
                .expect_err("export before seed must be refused");
            assert!(
                err.contains("no flow board"),
                "refusal names the missing board: {err}"
            );

            // ── Seed the golden board ─────────────────────────────────────
            let seeded = call(
                &ctx,
                "flow_seed_board",
                json!({ "graphId": graph_id, "json": GOLDEN_BOARD_JSON }),
            )
            .expect("seed golden board");
            assert_eq!(seeded["documentId"], "flow-board");
            assert_eq!(seeded["subjects"], 17, "{seeded}");
            assert_eq!(seeded["rows"]["systems"], 3);
            assert_eq!(seeded["rows"]["requirements"], 2);
            assert_eq!(seeded["rows"]["tasks"], 2);
            assert_eq!(seeded["rows"]["workflowLinks"], 1);
            assert_eq!(seeded["rows"]["deps"], 1);
            assert_eq!(seeded["extras"], json!([]), "golden has no extras");
            assert_eq!(
                seeded["derive"]["groupDeltas"]
                    .as_array()
                    .map(Vec::len),
                Some(2),
                "sys-avionics + sys-uav are the golden board's two groups: {seeded}"
            );
            assert_eq!(seeded["derive"]["cycles"], json!([]));

            // ── Live export: byte-exact vs reference + sidecar ────────────
            let reference = reference_export_json();
            let live = call(&ctx, "flow_export_board", json!({ "graphId": graph_id }))
                .expect("live export");
            let live_json = live["json"].as_str().expect("export json is a string");
            assert_eq!(
                live_json,
                with_sidecar(&reference, None, graph_id),
                "live export must be the reference canonical bytes + the null-restore-point sidecar"
            );
            assert_eq!(live["cycles"], json!([]));
            assert_eq!(live["derive"]["cycles"], json!([]));

            // Textual sidecar strip == reference — BYTE-EXACT leg.
            let sidecar_text = format!(
                ",\"sophia\":{{\"restorePointId\":null,\"graphId\":\"{graph_id}\",\"documentId\":\"flow-board\"}}"
            );
            let stripped = live_json.replacen(&sidecar_text, "", 1);
            assert_eq!(
                stripped, reference,
                "export minus the sidecar is byte-exact against the canonical reference"
            );

            // vs the raw golden fixture: NOT byte-exact (derived group
            // extents — G2's documented deviation) …
            assert_ne!(
                stripped, GOLDEN_BOARD_JSON,
                "golden's sys-uav stored extent fails containment; the derive corrects it, \
                 so raw bytes differ (documented: the byte-exact leg is vs the reference export)"
            );
            // … and equal EVERYWHERE else: parse both, delete `sophia`,
            // blank system extents, compare (compact re-serialization
            // equality via Value equality).
            let mut got: Value = serde_json::from_str(live_json).expect("export parses");
            got.as_object_mut().expect("object").remove("sophia");
            let mut want: Value =
                serde_json::from_str(GOLDEN_BOARD_JSON).expect("golden parses");
            blank_system_extents(&mut got);
            blank_system_extents(&mut want);
            assert_eq!(
                got, want,
                "export == golden minus the sophia key, outside the derived group extents"
            );

            // ── Restore point, then replace the live board ────────────────
            let rp = call(
                &ctx,
                "create_restore_point",
                json!({ "graphId": graph_id, "label": "g4-pin" }),
            )
            .expect("create restore point");
            let rp_id = rp["restorePointId"]
                .as_str()
                .expect("restore point id")
                .to_string();

            // Replace: golden minus task-2 (crate-wide `preserve_order`
            // keeps authored order through the parse→edit→serialize).
            let mut modified: Value =
                serde_json::from_str(GOLDEN_BOARD_JSON).expect("golden parses");
            let tasks = modified["tasks"].as_array_mut().expect("tasks array");
            tasks.retain(|row| row["id"] != "task-2");
            let modified_text = serde_json::to_string(&modified).expect("serialize");
            let reseeded = call(
                &ctx,
                "flow_seed_board",
                json!({ "graphId": graph_id, "json": modified_text, "replace": true }),
            )
            .expect("replace-seed modified board");
            assert_eq!(reseeded["rows"]["tasks"], 1);
            assert_eq!(reseeded["subjects"], 16);

            let live_after = call(&ctx, "flow_export_board", json!({ "graphId": graph_id }))
                .expect("live export after replace");
            assert!(
                !live_after["json"]
                    .as_str()
                    .expect("json string")
                    .contains("task-2"),
                "live board no longer carries task-2"
            );

            // ── Export FROM the restore point: pinned bytes, id in file ───
            let pinned = call(
                &ctx,
                "flow_export_board",
                json!({ "graphId": graph_id, "restorePointId": rp_id }),
            )
            .expect("restore point export");
            assert_eq!(
                pinned["json"].as_str().expect("json string"),
                with_sidecar(&reference, Some(&rp_id), graph_id),
                "restore-point export is the PRE-replace reference bytes with the \
                 restore point id travelling in the sidecar (FLOW-GARDEN-P1)"
            );

            let _ = std::fs::remove_dir_all(&jobs_dir);
        });
    }

    /// Seeding is a whole-board act: a non-empty board refuses a seed without
    /// `replace:true`, and the refused seed changes NOTHING (the live export
    /// is still byte-identical).
    #[test]
    fn seed_without_replace_on_a_non_empty_board_is_refused() {
        with_profile("garden-g4-flow-replace", || {
            let jobs_dir = std::env::temp_dir()
                .join(format!("garden-g4-flow-replace-jobs-{}", Uuid::new_v4()));
            let graph_id = "g4-flow-replace";
            let (app, jobs) = setup(graph_id, &jobs_dir);
            let ctx = McpCallCtx {
                app: app.clone(),
                jobs: &jobs,
            };

            call(
                &ctx,
                "flow_seed_board",
                json!({ "graphId": graph_id, "json": GOLDEN_BOARD_JSON }),
            )
            .expect("first seed on an empty board needs no replace flag");

            let err = call(
                &ctx,
                "flow_seed_board",
                json!({ "graphId": graph_id, "json": "{\"schemaVersion\":\"1\"}" }),
            )
            .expect_err("second seed without replace must be refused");
            assert!(
                err.contains("replace"),
                "refusal tells the caller about replace:true: {err}"
            );

            let live = call(&ctx, "flow_export_board", json!({ "graphId": graph_id }))
                .expect("live export after refused seed");
            assert_eq!(
                live["json"].as_str().expect("json string"),
                with_sidecar(&reference_export_json(), None, graph_id),
                "a refused seed must not have touched the board"
            );

            let explicit = call(
                &ctx,
                "flow_seed_board",
                json!({ "graphId": graph_id, "json": GOLDEN_BOARD_JSON, "replace": true }),
            )
            .expect("replace:true is the sanctioned whole-board replacement");
            assert_eq!(explicit["subjects"], 17);

            let _ = std::fs::remove_dir_all(&jobs_dir);
        });
    }

    /// The declaration files agree: both tools resolve through dispatch, are
    /// in the embedded catalog, and carry the brief's scopes (`flow.seed` =
    /// the document-write scope; export = the read scope).
    #[test]
    fn flow_tools_are_registered_with_scopes_in_every_source_of_truth() {
        assert!(lookup("flow_seed_board").is_some(), "dispatch row");
        assert!(lookup("flow_export_board").is_some(), "dispatch row");
        assert!(crate::mcp_tool_registry::is_known_mcp_tool_name(
            "flow_seed_board"
        ));
        assert!(crate::mcp_tool_registry::is_known_mcp_tool_name(
            "flow_export_board"
        ));
        assert_eq!(
            crate::loopback_scopes::mcp_tool_scopes("flow_seed_board"),
            vec!["documents.write.crdt"]
        );
        assert_eq!(
            crate::loopback_scopes::mcp_tool_scopes("flow_export_board"),
            vec!["documents.read"]
        );
        assert_eq!(
            crate::loopback_scopes::crdt_operation_scopes("flow.seed"),
            Some(vec!["documents.write.crdt"]),
            "the flow.seed op kind carries the document-write scope"
        );
    }
}

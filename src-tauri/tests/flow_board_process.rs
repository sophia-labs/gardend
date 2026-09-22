//! Unit G5 — the organism: a REAL compiled `gardend` headless process runs
//! the Mithras Flow seed rite end to end over 127.0.0.1. No mocks: the
//! literal artifact cloud-2 deploys (`cargo build --no-default-features
//! --features headless --example gardend`), a real on-disk profile + durable
//! dir, real HTTP `POST /mcp` `tools/call` requests — the exact transport a
//! gateway cell serves (`loopback_mcp_routes.rs`).
//!
//! The rite (brief, `build/contracts/interfaces.md` §H):
//!   1. boot gardend on a fresh profile (graph primed the production way —
//!      the gateway's `ensure_cell` creates the graph BEFORE the pod boots;
//!      `create_graph` over MCP is `mcpProfileDenied` in a cell, see
//!      `cell_graph_boundary_policy.json` — so `prime_graph` mirrors
//!      production exactly like `observatory_gardend_process.rs` does);
//!   2. `flow_seed_board` with the golden fixture;
//!   3. `sparql_query`: distinct subjects in `…:projection:flow` == 17 rows;
//!      `emporium_vocab` lists `flow`;
//!   4. the typed door, DIFFERENTIAL: `emporium_write` the same rows
//!      (geometry stripped, kind = class, localId = id, FKs resolved to the
//!      referenced row's minted subject IRI) under pack `flow` — `dry_run`
//!      accepted with zero violations, the real write leaves the projection
//!      lane's triple set byte-identical (the two doors agree: the seed's
//!      reconcile and the Emporium subject-scoped upsert converge on the
//!      same 84 triples), and a mutant record (illegal `flow:status`) HALTS
//!      with the lane untouched (all-or-halt);
//!   5. `create_restore_point` → `flow_export_board {restorePointId}` — the
//!      canonical bytes with the restore point id travelling in the
//!      `sophia` sidecar (FLOW-GARDEN-P1), equal to the live export modulo
//!      that one sidecar field; export == golden minus the sidecar
//!      everywhere except `systems[*].width/height` (G2's ratified
//!      FLOW-GARDEN-P2-amended derive deviation, carried by G4 —
//!      `flow_board_mcp.rs`'s dispatch tests pin the byte-exact leg against
//!      the canonical reference; this process test re-proves the parse-level
//!      equality and the confinement of the byte diff);
//!   6. SIGTERM (graceful, after the "durable flush enabled" fence —
//!      observatory_gardend_process.rs's own discipline), then RESTART on
//!      the same profile (cold hydrate) → both exports again → identical
//!      bytes (durability of the two roots through flush/hydrate).
//!
//! Red-first (brief, Acceptance): G5 depends on G4's tools, so the failing
//! direction is proven INSIDE the test instead of by a chronological red
//! run: (a) corrupt one byte of the final export and assert the byte
//! comparison fails; (b) the illegal-status mutant halts at the door and
//! the lane is unchanged.
//!
//! Wall-clock for boot, seed, export, restart is measured and printed as
//! `G5-RECEIPT` lines (run with `--nocapture` to see them in a green run).

// The shared process-test support crate (this file uses only its
// `gardend_process` half; `lease_authority` rides along unused — allow it).
#[cfg(unix)]
#[allow(dead_code)]
mod support;

#[cfg(unix)]
mod organism {
    use super::support::gardend_process::{
        self as process, GardendConfig, LoopbackEndpoint, ScratchDir,
    };
    use serde_json::{json, Map as JsonMap, Value};
    use std::collections::BTreeMap;
    use std::time::{Duration, Instant};

    const GOLDEN_BOARD_JSON: &str = include_str!("fixtures/flow/golden-board.json");
    const GRAPH_ID: &str = "flow-g5";
    /// 3 systems + 2 requirements + 2 tasks + 1 each of the other 10 tables.
    const GOLDEN_ROW_COUNT: usize = 17;
    /// 67 predicate triples (nulls skipped: req-2 dates ×2 + duration,
    /// task-1 due, task-2 start + duration, sys-uav parent — recounted from
    /// the golden fixture against `flow_board::vocab`) + 17 `rdf:type`.
    const GOLDEN_LANE_TRIPLES: usize = 84;

    fn graph_subject(graph_id: &str) -> String {
        format!("urn:mnemosyne:local:graph:{graph_id}")
    }

    fn lane_iri(graph_id: &str) -> String {
        format!("{}:projection:flow", graph_subject(graph_id))
    }

    // ── The wire-record mirror of contracts/vocabulary-map.md ────────────
    //
    // Duplicated from `src/emporium/flow_projection_tests.rs::table_map` as
    // literal data rather than imported: this is an EXTERNAL test crate (the
    // same discipline `observatory_gardend_process.rs` documents for its own
    // helpers), and for the differential leg an independent transcription of
    // the map is a feature — the projection door (G2/G3, `flow_board::vocab`)
    // and this record builder must agree through the vocabulary contract,
    // not through shared code.
    //
    // `(json_top_level_key, ClassName, kebab_subject_segment,
    //   [(ddl_column, wire_field_or_None_if_geometry, is_fk)])`
    type FieldRow = (&'static str, Option<&'static str>, bool);

    fn table_map() -> Vec<(&'static str, &'static str, &'static str, Vec<FieldRow>)> {
        vec![
            (
                "systems",
                "System",
                "system",
                vec![
                    ("parent_id", Some("parent"), true),
                    ("name", Some("name"), false),
                    ("description", Some("description"), false),
                    ("display_mode", Some("displayMode"), false),
                    ("color", Some("color"), false),
                    ("x", None, false),
                    ("y", None, false),
                    ("width", None, false),
                    ("height", None, false),
                ],
            ),
            (
                "requirements",
                "Requirement",
                "requirement",
                vec![
                    ("system_id", Some("ownedBy"), true),
                    ("text", Some("text"), false),
                    ("source", Some("requirementSource"), false),
                    ("status", Some("status"), false),
                    ("sort_order", Some("sortOrder"), false),
                    ("start_date", Some("startDate"), false),
                    ("due_date", Some("dueDate"), false),
                    ("duration_days", Some("durationDays"), false),
                ],
            ),
            (
                "tasks",
                "Task",
                "task",
                vec![
                    ("owner_type", Some("ownerType"), false),
                    ("owner_id", Some("ownedBy"), true),
                    ("text", Some("text"), false),
                    ("done", Some("done"), false),
                    ("sort_order", Some("sortOrder"), false),
                    ("start_date", Some("startDate"), false),
                    ("due_date", Some("dueDate"), false),
                    ("duration_days", Some("durationDays"), false),
                ],
            ),
            (
                "workflows",
                "Workflow",
                "workflow",
                vec![
                    ("name", Some("name"), false),
                    ("description", Some("description"), false),
                    ("x", None, false),
                    ("y", None, false),
                ],
            ),
            (
                "workflowLinks",
                "WorkflowLink",
                "workflow-link",
                vec![
                    ("workflow_id", Some("workflow"), true),
                    ("system_id", Some("system"), true),
                    ("role", Some("role"), false),
                    ("waypoints", None, false),
                ],
            ),
            (
                "outcomes",
                "Outcome",
                "outcome",
                vec![
                    ("workflow_id", Some("workflow"), true),
                    ("text", Some("text"), false),
                    ("achieved", Some("achieved"), false),
                    ("sort_order", Some("sortOrder"), false),
                ],
            ),
            (
                "trades",
                "Trade",
                "trade",
                vec![
                    ("name", Some("name"), false),
                    ("description", Some("tradeRationale"), false),
                    ("winner_id", Some("winner"), true),
                    ("x", None, false),
                    ("y", None, false),
                ],
            ),
            (
                "tradeLinks",
                "TradeLink",
                "trade-link",
                vec![
                    ("trade_id", Some("trade"), true),
                    ("system_id", Some("system"), true),
                    ("waypoints", None, false),
                ],
            ),
            (
                "constraints",
                "Constraint",
                "constraint",
                vec![
                    ("name", Some("name"), false),
                    ("description", Some("description"), false),
                    ("x", None, false),
                    ("y", None, false),
                ],
            ),
            (
                "constraintLinks",
                "ConstraintLink",
                "constraint-link",
                vec![
                    ("constraint_id", Some("constraint"), true),
                    ("target_id", Some("target"), true),
                    ("note", Some("note"), false),
                    ("waypoints", None, false),
                ],
            ),
            (
                "sources",
                "Source",
                "source",
                vec![
                    ("owner_id", Some("ownedBy"), true),
                    ("text", Some("sourceCitation"), false),
                    ("sort_order", Some("sortOrder"), false),
                ],
            ),
            (
                "edges",
                "Edge",
                "edge",
                vec![
                    ("source_id", Some("sourceNode"), true),
                    ("target_id", Some("targetNode"), true),
                    ("label", Some("label"), false),
                    ("waypoints", None, false),
                ],
            ),
            (
                "deps",
                "Dependency",
                "dependency",
                vec![
                    ("predecessor_id", Some("predecessor"), true),
                    ("successor_id", Some("successor"), true),
                ],
            ),
        ]
    }

    /// Golden board rows → `emporium_write` wire records: geometry stripped,
    /// `kind` = class, `localId` = id, nulls omitted, every FK resolved to
    /// the REFERENCED row's minted subject IRI
    /// (`{graph_subject}:projection:flow:{kebab}:{localId}`) — the same
    /// resolution `flow_board::board_desired_triples` performs, done here
    /// independently from the contract.
    fn flow_records(graph_id: &str) -> Vec<Value> {
        let board: Value =
            serde_json::from_str(GOLDEN_BOARD_JSON).expect("golden-board.json parses");
        let gs = graph_subject(graph_id);
        let tables = table_map();

        let mut subject_of: BTreeMap<String, String> = BTreeMap::new();
        for (json_key, _class, kebab, _fields) in &tables {
            for row in board[json_key].as_array().expect("table is an array") {
                let id = row["id"].as_str().expect("row has an id");
                subject_of.insert(
                    id.to_string(),
                    format!("{gs}:projection:flow:{kebab}:{id}"),
                );
            }
        }

        let mut out = Vec::new();
        for (json_key, class, _kebab, fields) in &tables {
            for row in board[json_key].as_array().expect("table is an array") {
                let id = row["id"].as_str().expect("row has an id");
                let mut record = JsonMap::new();
                record.insert("kind".to_string(), json!(class));
                record.insert("localId".to_string(), json!(id));
                for (column, wire_field, is_fk) in fields {
                    let Some(wire_field) = wire_field else {
                        continue; // geometry — excluded (interfaces.md §C)
                    };
                    let value = &row[*column];
                    if value.is_null() {
                        continue; // nullable + optional — omit, never write null
                    }
                    if *is_fk {
                        let raw_id = value.as_str().unwrap_or_else(|| {
                            panic!("{class}.{column} on row {id}: non-string FK")
                        });
                        let resolved = subject_of.get(raw_id).unwrap_or_else(|| {
                            panic!("{class}.{column} on row {id}: unknown id {raw_id}")
                        });
                        record.insert(wire_field.to_string(), json!(resolved));
                    } else {
                        record.insert(wire_field.to_string(), value.clone());
                    }
                }
                out.push(Value::Object(record));
            }
        }
        out
    }

    // ── MCP over the real loopback HTTP transport ────────────────────────

    async fn mcp_call(
        client: &reqwest::Client,
        endpoint: &LoopbackEndpoint,
        name: &str,
        arguments: Value,
    ) -> Result<Value, String> {
        let body = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "tools/call",
            "params": { "name": name, "arguments": arguments },
        });
        let response = client
            .post(format!("http://127.0.0.1:{}/mcp", endpoint.port))
            .bearer_auth(&endpoint.token)
            .json(&body)
            .send()
            .await
            .map_err(|error| format!("POST /mcp ({name}): {error}"))?;
        let status = response.status();
        let envelope: Value = response
            .json()
            .await
            .map_err(|error| format!("parse /mcp response ({name}): {error}"))?;
        if !status.is_success() {
            return Err(format!("/mcp {name}: HTTP {status}: {envelope}"));
        }
        if let Some(error) = envelope.get("error") {
            return Err(error
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("tool error with no message")
                .to_string());
        }
        envelope
            .get("result")
            .and_then(|result| result.get("structuredContent"))
            .cloned()
            .ok_or_else(|| format!("/mcp {name}: no structuredContent in {envelope}"))
    }

    async fn mcp_ok(
        client: &reqwest::Client,
        endpoint: &LoopbackEndpoint,
        name: &str,
        arguments: Value,
    ) -> Value {
        mcp_call(client, endpoint, name, arguments)
            .await
            .unwrap_or_else(|error| panic!("{name} failed: {error}"))
    }

    /// The full `:projection:flow` triple set, as SPARQL rows ordered by
    /// `(?s, ?p, ?o)` — the lane snapshot both differential legs compare.
    async fn lane_snapshot(
        client: &reqwest::Client,
        endpoint: &LoopbackEndpoint,
        graph_id: &str,
    ) -> Vec<Value> {
        let lane = lane_iri(graph_id);
        let result = mcp_ok(
            client,
            endpoint,
            "sparql_query",
            json!({
                "graphId": graph_id,
                "query": format!(
                    "SELECT ?s ?p ?o WHERE {{ GRAPH <{lane}> {{ ?s ?p ?o }} }} ORDER BY ?s ?p ?o"
                ),
                "maxRows": 10000,
            }),
        )
        .await;
        result["rows"]
            .as_array()
            .unwrap_or_else(|| panic!("sparql rows array, got {result}"))
            .clone()
    }

    async fn distinct_lane_subjects(
        client: &reqwest::Client,
        endpoint: &LoopbackEndpoint,
        graph_id: &str,
    ) -> usize {
        let lane = lane_iri(graph_id);
        let result = mcp_ok(
            client,
            endpoint,
            "sparql_query",
            json!({
                "graphId": graph_id,
                "query": format!(
                    "SELECT DISTINCT ?s WHERE {{ GRAPH <{lane}> {{ ?s ?p ?o }} }}"
                ),
                "maxRows": 10000,
            }),
        )
        .await;
        result["rows"].as_array().map(Vec::len).unwrap_or(0)
    }

    /// I1 evidence printer: base64 wrapped at 76 cols between BEGIN/END
    /// markers (log-line safe; the integration seat decodes and
    /// byte-compares locally). Print-only — asserts nothing.
    fn print_b64_block(label: &str, bytes: &[u8]) {
        use base64::Engine as _;
        let encoded = base64::engine::general_purpose::STANDARD.encode(bytes);
        println!("{label}-B64-BEGIN");
        for chunk in encoded.as_bytes().chunks(76) {
            println!("{}", std::str::from_utf8(chunk).expect("base64 is ASCII"));
        }
        println!("{label}-B64-END");
    }

    fn spawn(
        root: &ScratchDir,
        label: &str,
        graph_id: &str,
    ) -> (process::ChildGuard, std::path::PathBuf) {
        process::spawn_gardend(GardendConfig {
            profile_dir: root.child("profile"),
            durable_dir: root.child("durable"),
            graph_id: graph_id.into(),
            loopback_host: "127.0.0.1".into(),
            loopback_port: 0,
            extra_env: [
                ("RUST_LOG", "info"),
                // Keep the cell warm across the whole test; the restart is
                // OURS (SIGTERM), never an idle reap.
                ("GARDEN_IDLE_TTL_SECONDS", "3600"),
                // Fast dirty-driven durable publishes (the cell_lease tests'
                // own settings) so the graceful shutdown's final flush and
                // the restart hydrate exercise a CURRENT durable snapshot.
                ("GARDEN_FLUSH_DEBOUNCE_SECONDS", "1"),
                ("GARDEN_FLUSH_INTERVAL_SECONDS", "1"),
            ]
            .into_iter()
            .map(|(key, value)| (key.to_string(), value.to_string()))
            .collect(),
            log_dir: root.child("logs"),
            log_label: label.into(),
        })
    }

    fn endpoint_or_dump_logs(root: &ScratchDir, label: &str) -> LoopbackEndpoint {
        let profile = root.child("profile");
        LoopbackEndpoint::from_profile(&profile, Duration::from_secs(60)).unwrap_or_else(|| {
            panic!(
                "gardend ({label}) never published a fresh loopback manifest\n\
                 --- stdout ---\n{}\n--- stderr ---\n{}",
                process::read_log(&root.child("logs"), label, "stdout"),
                process::read_log(&root.child("logs"), label, "stderr"),
            )
        })
    }

    fn blank_system_extents(value: &mut Value) {
        if let Some(systems) = value.get_mut("systems").and_then(Value::as_array_mut) {
            for row in systems {
                if let Some(object) = row.as_object_mut() {
                    object.insert("width".to_string(), Value::Null);
                    object.insert("height".to_string(), Value::Null);
                }
            }
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn flow_board_organism_runs_the_seed_rite_end_to_end_through_a_real_gardend_process() {
        let root = ScratchDir::new("flow-g5-organism");
        let profile = root.child("profile");
        let logs = root.child("logs");

        // The gateway creates the graph BEFORE the pod boots (§A.5 of the
        // observatory process test's module doc; `create_graph` over MCP is
        // mcpProfileDenied in a cell) — mirror production.
        process::prime_graph(&profile, GRAPH_ID);

        // ── 1. Boot the real compiled gardend ────────────────────────────
        let boot_started = Instant::now();
        let (mut child, _log_dir) = spawn(&root, "run1", GRAPH_ID);
        let endpoint = endpoint_or_dump_logs(&root, "run1");
        let client = reqwest::Client::new();
        let (health_status, health) = endpoint.health().await;
        assert!(
            health_status.is_success(),
            "gardend /health must be green after boot: {health_status} {health}"
        );
        let boot_ms = boot_started.elapsed().as_millis();
        assert!(
            child.is_running(),
            "gardend must still be alive after readiness"
        );

        // ── 2. Seed the golden board through the real MCP transport ──────
        let seed_started = Instant::now();
        let seeded = mcp_ok(
            &client,
            &endpoint,
            "flow_seed_board",
            json!({ "graphId": GRAPH_ID, "json": GOLDEN_BOARD_JSON }),
        )
        .await;
        let seed_ms = seed_started.elapsed().as_millis();
        assert_eq!(seeded["documentId"], "flow-board", "{seeded}");
        assert_eq!(seeded["subjects"], GOLDEN_ROW_COUNT, "{seeded}");

        // ── 3. The projection lane + the vocab catalog ───────────────────
        assert_eq!(
            distinct_lane_subjects(&client, &endpoint, GRAPH_ID).await,
            GOLDEN_ROW_COUNT,
            "distinct subjects in :projection:flow == total golden rows"
        );
        let vocabs = mcp_ok(&client, &endpoint, "emporium_vocab", json!({})).await;
        let flow_listed = vocabs["vocabularies"]
            .as_array()
            .unwrap_or_else(|| panic!("emporium_vocab catalog: {vocabs}"))
            .iter()
            .any(|entry| entry["name"] == "flow");
        assert!(flow_listed, "emporium_vocab must list the flow pack: {vocabs}");

        // ── 4. The typed door, differential ──────────────────────────────
        let lane_before = lane_snapshot(&client, &endpoint, GRAPH_ID).await;
        assert_eq!(
            lane_before.len(),
            GOLDEN_LANE_TRIPLES,
            "the seed's reconcile lane carries the full golden triple set"
        );

        // I1 differential (b) evidence: the lane's REAL triple set from the
        // cell store, dumped as N-Triples through the rdf_dump door and
        // printed base64-wrapped so the integration seat can byte-compare it
        // (after canonical N-Triples sort) against S5's render turtle. Print
        // only — every behavioral assertion above/below is unchanged.
        let dump = mcp_ok(
            &client,
            &endpoint,
            "rdf_dump",
            json!({
                "graphId": GRAPH_ID,
                "format": "n-triples",
                "sourceGraphIri": lane_iri(GRAPH_ID),
            }),
        )
        .await;
        let lane_nt = dump["data"].as_str().expect("rdf_dump data string");
        assert_eq!(
            lane_nt.lines().filter(|line| !line.trim().is_empty()).count(),
            GOLDEN_LANE_TRIPLES,
            "the N-Triples dump of the lane carries the same triple count as \
             the SPARQL snapshot"
        );
        print_b64_block("I1-LANE-NT", lane_nt.as_bytes());

        let records = flow_records(GRAPH_ID);
        assert_eq!(records.len(), GOLDEN_ROW_COUNT);

        // dry_run: accepted, zero violations.
        let preview = mcp_ok(
            &client,
            &endpoint,
            "emporium_write",
            json!({
                "graphId": GRAPH_ID,
                "vocab": "flow",
                "records": records,
                "dryRun": true,
            }),
        )
        .await;
        assert_eq!(preview["ok"], true, "dry_run must accept the golden rows: {preview}");
        assert_eq!(preview["dryRun"], true, "{preview}");
        assert!(
            preview["violations"].is_null(),
            "dry_run must report zero violations: {preview}"
        );
        assert!(
            preview["results"]
                .as_array()
                .expect("results array")
                .iter()
                .all(|outcome| outcome["outcome"] == "applied"),
            "every previewed record must be applied: {preview}"
        );

        // For real, into the SAME graph: the lane's triple set is unchanged
        // (the two doors agree — the subject-scoped upsert converges).
        let written = mcp_ok(
            &client,
            &endpoint,
            "emporium_write",
            json!({
                "graphId": GRAPH_ID,
                "vocab": "flow",
                "records": flow_records(GRAPH_ID),
            }),
        )
        .await;
        assert_eq!(written["ok"], true, "real write must apply: {written}");
        assert!(
            written["results"]
                .as_array()
                .expect("results array")
                .iter()
                .all(|outcome| outcome["outcome"] == "applied"),
            "every written record must be applied: {written}"
        );
        let lane_after = lane_snapshot(&client, &endpoint, GRAPH_ID).await;
        assert_eq!(
            lane_before, lane_after,
            "emporium_write of the projection's own rows must leave the \
             :projection:flow triple set byte-identical (the doors agree)"
        );

        // A mutant record — illegal flow:status — HALTS, lane unchanged.
        let mut mutant_records = flow_records(GRAPH_ID);
        {
            let req_1 = mutant_records
                .iter_mut()
                .find(|record| record["localId"] == "req-1")
                .expect("req-1 record exists");
            req_1["status"] = json!("cancelled"); // not in sh:in("open","met","waived")
        }
        let mutant_preview = mcp_ok(
            &client,
            &endpoint,
            "emporium_write",
            json!({
                "graphId": GRAPH_ID,
                "vocab": "flow",
                "records": mutant_records.clone(),
                "dryRun": true,
            }),
        )
        .await;
        assert_eq!(
            mutant_preview["ok"], false,
            "illegal status must halt the dry_run: {mutant_preview}"
        );
        let violations_text = mutant_preview["violations"].to_string();
        assert!(
            violations_text.contains("status"),
            "the violation names flow:status: {mutant_preview}"
        );
        let mutant_write = mcp_ok(
            &client,
            &endpoint,
            "emporium_write",
            json!({
                "graphId": GRAPH_ID,
                "vocab": "flow",
                "records": mutant_records,
            }),
        )
        .await;
        assert_eq!(
            mutant_write["ok"], false,
            "illegal status must halt the real write: {mutant_write}"
        );
        assert!(
            mutant_write["results"]
                .as_array()
                .expect("results array")
                .iter()
                .all(|outcome| outcome["outcome"] == "halted"),
            "all-or-halt: every record in the mutant batch halts: {mutant_write}"
        );
        assert_eq!(
            lane_snapshot(&client, &endpoint, GRAPH_ID).await,
            lane_before,
            "a halted batch must leave the lane untouched"
        );

        // ── 5. Restore point → export, sidecar id travelling in the file ─
        let restore_point = mcp_ok(
            &client,
            &endpoint,
            "create_restore_point",
            json!({ "graphId": GRAPH_ID, "label": "g5-organism-pin" }),
        )
        .await;
        let restore_point_id = restore_point["restorePointId"]
            .as_str()
            .unwrap_or_else(|| panic!("restorePointId in {restore_point}"))
            .to_string();

        let export_started = Instant::now();
        let live = mcp_ok(
            &client,
            &endpoint,
            "flow_export_board",
            json!({ "graphId": GRAPH_ID }),
        )
        .await;
        let export_ms = export_started.elapsed().as_millis();
        let live_json = live["json"].as_str().expect("export json string").to_string();
        // I1 differential (a)/seed-rite evidence: the cell's live export
        // bytes, verbatim (the seat compares canonicalize(golden) against
        // these minus the sidecar). Print-only.
        print_b64_block("I1-LIVE-EXPORT", live_json.as_bytes());

        // The sidecar: restorePointId null on the live export, the graph and
        // constant document id present (interfaces.md §E).
        let live_value: Value = serde_json::from_str(&live_json).expect("live export parses");
        assert_eq!(
            live_value["sophia"],
            json!({
                "restorePointId": null,
                "graphId": GRAPH_ID,
                "documentId": "flow-board"
            }),
            "live sidecar shape"
        );

        // Export == golden minus the sidecar — everywhere except the
        // FLOW-GARDEN-P2-amended group extents (G2's ratified deviation,
        // carried through G4: golden's own sys-uav stored 420x280 fails the
        // fit-to-content containment and the derive corrects it at export).
        let mut got = live_value.clone();
        got.as_object_mut().expect("export object").remove("sophia");
        let mut want: Value =
            serde_json::from_str(GOLDEN_BOARD_JSON).expect("golden parses");
        let sidecar_text = format!(
            ",\"sophia\":{{\"restorePointId\":null,\"graphId\":\"{GRAPH_ID}\",\"documentId\":\"flow-board\"}}"
        );
        let stripped = live_json.replacen(&sidecar_text, "", 1);
        assert_ne!(
            stripped, live_json,
            "the live export carries exactly the expected sidecar bytes"
        );
        assert_ne!(
            stripped, GOLDEN_BOARD_JSON,
            "raw bytes differ from golden ONLY via the documented P2-amended \
             extent correction — asserted next by parse-equality after \
             blanking system extents"
        );
        blank_system_extents(&mut got);
        blank_system_extents(&mut want);
        assert_eq!(
            got, want,
            "export == golden minus the sophia sidecar, outside the derived \
             group extents (the G2 deviation of record)"
        );

        // Export FROM the restore point: identical to the live export except
        // the restore point id travels in the sidecar (FLOW-GARDEN-P1).
        let pinned = mcp_ok(
            &client,
            &endpoint,
            "flow_export_board",
            json!({ "graphId": GRAPH_ID, "restorePointId": restore_point_id }),
        )
        .await;
        let pinned_json = pinned["json"].as_str().expect("pinned json string");
        let expected_pinned = live_json.replacen(
            "\"restorePointId\":null",
            &format!("\"restorePointId\":\"{restore_point_id}\""),
            1,
        );
        assert_eq!(
            pinned_json, expected_pinned,
            "restore-point export == live bytes with the created restore \
             point id in the sidecar"
        );

        // ── 6. Restart on the same profile: cold hydrate, identical bytes ─
        // Graceful-shutdown fence first (observatory_gardend_process.rs):
        // SIGTERM before gardend arms its signal handler hits the OS default
        // disposition. "durable flush enabled" is the last boot log line
        // before wait_for_shutdown.
        assert!(
            process::wait_for_log(&logs, "run1", "stderr", "durable flush enabled", Duration::from_secs(15)),
            "gardend never logged \"durable flush enabled\" — cannot safely \
             SIGTERM\n--- stderr ---\n{}",
            process::read_log(&logs, "run1", "stderr"),
        );
        let restart_started = Instant::now();
        process::send_sigterm(&child.child);
        let exit = process::wait_with_timeout(&mut child.child, Duration::from_secs(30))
            .unwrap_or_else(|| {
                panic!(
                    "gardend did not exit within 30s of SIGTERM\n--- stderr ---\n{}",
                    process::read_log(&logs, "run1", "stderr"),
                )
            });
        assert!(
            exit.success(),
            "gardend must exit 0 on graceful SIGTERM, got {exit:?}\n--- stderr ---\n{}",
            process::read_log(&logs, "run1", "stderr"),
        );
        let shutdown_ms = restart_started.elapsed().as_millis();
        // Drop the first guard BEFORE respawning: its Drop clears the
        // profile→pid registration the manifest-freshness check keys on.
        drop(child);

        let reboot_started = Instant::now();
        let (mut child2, _log_dir2) = spawn(&root, "run2", GRAPH_ID);
        let endpoint2 = endpoint_or_dump_logs(&root, "run2");
        let reboot_ms = reboot_started.elapsed().as_millis();
        assert!(child2.is_running(), "restarted gardend must be alive");

        let live_after_restart = mcp_ok(
            &client,
            &endpoint2,
            "flow_export_board",
            json!({ "graphId": GRAPH_ID }),
        )
        .await;
        let live_after_restart_json = live_after_restart["json"]
            .as_str()
            .expect("post-restart export json string");
        assert_eq!(
            live_after_restart_json, live_json,
            "cold hydrate must reproduce the live export byte-for-byte \
             (durability of both named roots through flush/hydrate)"
        );

        // The restore-point root also survives the restart.
        let pinned_after_restart = mcp_ok(
            &client,
            &endpoint2,
            "flow_export_board",
            json!({ "graphId": GRAPH_ID, "restorePointId": restore_point_id }),
        )
        .await;
        assert_eq!(
            pinned_after_restart["json"].as_str().expect("json string"),
            expected_pinned,
            "restore-point export survives the restart byte-for-byte"
        );

        // The projection lane survives hydrate too (scout §2: the flow lane
        // is re-materialized/persisted, never cleared by reseed or hydrate).
        assert_eq!(
            distinct_lane_subjects(&client, &endpoint2, GRAPH_ID).await,
            GOLDEN_ROW_COUNT,
            "the :projection:flow lane survives the restart"
        );

        // ── Red-direction proof (brief, Acceptance): the byte comparison
        // actually bites — corrupt ONE byte and the equality fails.
        let mut corrupted = live_after_restart_json.as_bytes().to_vec();
        let mid = corrupted.len() / 2;
        corrupted[mid] = corrupted[mid].wrapping_add(1);
        assert_ne!(
            corrupted.as_slice(),
            live_json.as_bytes(),
            "a single corrupted byte must break the export comparison — the \
             equality above is a real byte-level check, not a vacuous one"
        );

        // ── The measured receipt rows ────────────────────────────────────
        eprintln!("G5-RECEIPT boot_ms={boot_ms}");
        eprintln!("G5-RECEIPT seed_ms={seed_ms}");
        eprintln!("G5-RECEIPT export_ms={export_ms}");
        eprintln!("G5-RECEIPT shutdown_ms={shutdown_ms}");
        eprintln!("G5-RECEIPT reboot_ms={reboot_ms}");
        eprintln!("G5-RECEIPT restart_ms={}", shutdown_ms + reboot_ms);

        let _ = process::wait_with_timeout(&mut child2.child, Duration::from_secs(0));
    }
}

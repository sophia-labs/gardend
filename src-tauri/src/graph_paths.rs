use crate::app_runtime::AppHandle;
use crate::{
    graph_service::GraphRecord,
    ids::validate_local_id,
    profile_paths::graphs_dir,
    runtime_config::GRAPH_STATUS_DELETED,
    semantic_index_paths::semantic_index_dir,
    storage::{create_dir_all, read_json},
    ydoc_paths::workspace_ydoc_dir,
};
use std::path::{Path, PathBuf};
#[cfg(all(feature = "headless", not(feature = "desktop")))]
use std::sync::Arc;
#[cfg(all(feature = "headless", not(feature = "desktop")))]
use std::sync::{Mutex, OnceLock};
#[cfg(all(feature = "headless", not(feature = "desktop")))]
#[cfg(feature = "desktop")]
use tauri::Manager;

#[cfg(all(feature = "headless", not(feature = "desktop")))]
static GRAPH_SELF_HEAL_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

#[cfg(all(feature = "headless", not(feature = "desktop")))]
fn materialization_event_for_lifecycle(lifecycle: &str) -> &'static str {
    if lifecycle == "provisioning" {
        "graph.initialize"
    } else {
        "graph.repair"
    }
}

#[cfg(all(feature = "headless", not(feature = "desktop")))]
fn materialization_event() -> Result<
    (
        &'static str,
        crate::cell_registry_authority::CellRegistryBinding,
    ),
    String,
> {
    let binding = crate::cell_registry_authority::verified_binding()?
        .ok_or_else(|| "cell graph registry binding is unavailable".to_string())?;
    let event = materialization_event_for_lifecycle(&binding.lifecycle_state);
    Ok((event, binding))
}

#[cfg(all(feature = "headless", not(feature = "desktop")))]
fn authorize_cell_self_heal(app: &AppHandle, graph_id: &str) -> Result<(), String> {
    let boundary = app
        .try_state::<Arc<crate::cell_graph_boundary::CellGraphBoundary>>()
        .ok_or_else(|| "cell graph boundary is not initialized".to_string())?;
    boundary.authorize_graph_self_heal(graph_id)
}

/// Self-heal a missing `graph.json` (F4c). Compiled only into a pure headless
/// (cell) binary — `headless` without `desktop`, since the two are composable
/// cargo features and a desktop+headless build must never carry self-heal —
/// and only acts when the gateway has told us we're a gateway-fronted cell
/// (`GARDEN_SELF_HEAL_GRAPHS=1`) — see `runtime_config::self_heal_graphs_enabled()`.
/// No-op if the graph already exists (including a concurrent racer having
/// just created it).
///
/// Callers must check `!graph_path.is_file()` themselves before invoking this
/// — a soft-deleted graph (`GRAPH_STATUS_DELETED`) must never reach here, or
/// it would be silently resurrected. See `existing_graph_dir` /
/// `graph_record_store::read_graph_record`.
///
/// SECURITY: never call this from a surface reachable without gateway auth
/// (e.g. the anonymous query-signed image read/validation path) — a missing
/// graph there must 404, not self-heal. Use `existing_graph_dir_no_heal` for
/// those. See F4c security review finding 1.
#[cfg(all(feature = "headless", not(feature = "desktop")))]
pub(crate) fn self_heal_missing_graph(app: &AppHandle, graph_id: &str) -> Result<(), String> {
    if !crate::runtime_config::self_heal_graphs_enabled() {
        return Err(format!("graph not found: {graph_id}"));
    }
    // The gateway authorizes the accessor; the process-bound cell authority
    // constrains which namespace F4c may materialize. Requiring managed state
    // here also makes startup-order regressions fail closed instead of falling
    // back to the old caller-selected namespace.
    authorize_cell_self_heal(app, graph_id)?;
    let _guard = GRAPH_SELF_HEAL_LOCK
        .get_or_init(|| Mutex::new(()))
        .lock()
        .map_err(|_| "graph self-heal lock poisoned".to_string())?;

    // Re-check under the lock: a racing request may have already healed it.
    let graph_path = graphs_dir(app)?.join(graph_id).join("graph.json");
    if graph_path.is_file() {
        return Ok(());
    }

    log::warn!("F4c self-heal: creating missing graph store for {graph_id}");
    // Authenticated headless request roots acquire the graph persistence lease
    // before reaching this self-heal. Use the non-reentrant create body so a
    // missing graph cannot deadlock trying to reacquire its own lifecycle gate.
    match crate::graph_service::create_graph_service_inner(
        app,
        crate::graph_service::CreateGraphInput {
            title: graph_id.to_string(),
            graph_id: Some(graph_id.to_string()),
            description: Some("auto-created by cell self-heal (F4c)".to_string()),
            operation_id: None,
        },
    ) {
        Ok(_) => {
            let (event, binding) = materialization_event()?;
            log::info!(
                target: "audit",
                "event={event} owner={} graph={} generation={} registry_revision={}",
                binding.owner,
                binding.graph_id,
                binding.generation,
                binding.registry_revision,
            );
            Ok(())
        }
        // Lost the race to a concurrent legitimate creator (e.g. the
        // gateway's own async F4b create) — the record now exists for real,
        // with the real title. Don't clobber it; just proceed.
        Err(error) if error.kind() == crate::app_error::AppErrorKind::Conflict => Ok(()),
        Err(error) => Err(error.message()),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum GraphDirOutcome {
    Existing,
    SelfHealed,
}

pub(crate) fn existing_graph_dir_reporting_heal(
    app: &AppHandle,
    graph_id: &str,
) -> Result<(PathBuf, GraphDirOutcome), String> {
    validate_local_id(graph_id, "graph_id")?;
    let graph_dir = graphs_dir(app)?.join(graph_id);
    let graph_path = graph_dir.join("graph.json");
    let outcome = if graph_path.is_file() {
        GraphDirOutcome::Existing
    } else {
        // sirin-aduanera diagnostic, 2026-09-04: this exact fork — missing
        // graph.json, first access — is where a live Sirin cell diverges from
        // every faithful local reproduction of F4c (see the report: the same
        // env a real owner-scoped cell carries, replayed in-process, DOES
        // self-heal; three independent live graphs, through both creation
        // doors, did NOT). Unconditional and cheap (this branch only runs
        // once per graph's lifetime — the moment it self-heals or is denied),
        // so the NEXT occurrence is diagnosable from the ordinary cell log
        // alone, without guessing again: if self-heal is enabled this line is
        // immediately followed by graph_paths' own "F4c self-heal: creating
        // missing graph store for {id}" WARN; if that second line is ever
        // absent although this one logged enabled=true, the gap is inside
        // self_heal_missing_graph/create_graph_service_inner, not the flag.
        log::warn!(
            "existing_graph_dir: {graph_id} has no graph.json; self_heal_graphs_enabled={}",
            crate::runtime_config::self_heal_graphs_enabled()
        );
        #[cfg(all(feature = "headless", not(feature = "desktop")))]
        {
            self_heal_missing_graph(app, graph_id)?;
            GraphDirOutcome::SelfHealed
        }
        #[cfg(any(not(feature = "headless"), feature = "desktop"))]
        {
            return Err(format!("graph not found: {graph_id}"));
        }
    };
    let graph = read_json::<GraphRecord>(&graph_path)?;
    if graph.status == GRAPH_STATUS_DELETED {
        return Err(format!("graph not found: {graph_id}")); // never self-healed
    }
    ensure_graph_content_dirs(&graph_dir)?;
    Ok((graph_dir, outcome))
}

pub(crate) fn existing_graph_dir(app: &AppHandle, graph_id: &str) -> Result<PathBuf, String> {
    existing_graph_dir_reporting_heal(app, graph_id).map(|(graph_dir, _)| graph_dir)
}

/// Read-only probe: does this graph's `graph.json` exist right now? The
/// room-open escalation uses this under its shared lease so the existence
/// check and the heal share one path construction instead of drifting apart.
pub(crate) fn graph_json_present(app: &AppHandle, graph_id: &str) -> Result<bool, String> {
    validate_local_id(graph_id, "graph_id")?;
    Ok(graphs_dir(app)?.join(graph_id).join("graph.json").is_file())
}

/// Non-healing counterpart to `existing_graph_dir` — the original
/// pre-self-heal lookup logic: a missing (or soft-deleted)
/// `graph.json` is always `Err`, never created. Use this for any surface
/// reachable WITHOUT gateway auth, where self-heal would let an attacker
/// materialize an arbitrary graph merely by hitting it with a guessed
/// `graph_id` — e.g. the anonymous query-signed image read/validation path
/// (`original_file_access_tokens::image_access_token_matches` and
/// `original_file_service::read_image_file`). A missing graph means no valid
/// image token (or anything else gated on the graph existing) can exist
/// anyway, so refusing to self-heal here changes no legitimate behavior.
/// See F4c security review finding 1.
pub(crate) fn existing_graph_dir_no_heal(
    app: &AppHandle,
    graph_id: &str,
) -> Result<PathBuf, String> {
    validate_local_id(graph_id, "graph_id")?;
    let graph_dir = graphs_dir(app)?.join(graph_id);
    let graph_path = graph_dir.join("graph.json");
    if !graph_path.is_file() {
        return Err(format!("graph not found: {graph_id}"));
    }
    let graph = read_json::<GraphRecord>(&graph_path)?;
    if graph.status == GRAPH_STATUS_DELETED {
        return Err(format!("graph not found: {graph_id}"));
    }
    ensure_graph_content_dirs(&graph_dir)?;
    Ok(graph_dir)
}

pub(crate) fn documents_dir(graph_dir: &Path) -> PathBuf {
    graph_dir.join("documents")
}

pub(crate) fn artifacts_dir(graph_dir: &Path) -> PathBuf {
    graph_dir.join("artifacts")
}

pub(crate) fn images_dir(graph_dir: &Path) -> PathBuf {
    graph_dir.join("images")
}

pub(crate) fn ensure_graph_content_dirs(graph_dir: &Path) -> Result<(), String> {
    create_dir_all(&documents_dir(graph_dir))?;
    create_dir_all(&artifacts_dir(graph_dir))?;
    create_dir_all(&graph_dir.join("ydocs/documents"))?;
    create_dir_all(&workspace_ydoc_dir(graph_dir))?;
    Ok(())
}

#[cfg(all(test, feature = "headless", not(feature = "desktop")))]
mod cell_self_heal_tests {
    use super::*;

    /// sirin-aduanera diagnostic, 2026-09-04: end-to-end proof (or refutation)
    /// of F4c for the exact scenario reported against Sirin — a gateway-spawned
    /// cell, freshly bound to a graph_id that has never been created, whose
    /// FIRST resolution of that graph's directory is `existing_graph_dir`
    /// (the same call `dump_rdf_service`/`run_sparql_query_service` make for
    /// `rdf_dump`/`sparql_query`). Nothing like this ran before:
    /// `heal_escalation_missing_graph_open_runs_heal_under_exclusive`
    /// (observatory_gardend_process.rs) is `#[ignore]` with a `todo!()` body,
    /// so F4c's happy path had never actually been exercised against real
    /// code, only asserted about in comments. Isolate this test with
    /// `cargo test ... -- --exact` (see run transcript in the report): it
    /// mutates the process-global `GARDEN_SELF_HEAL_GRAPHS` env var into the
    /// `OnceLock` that `runtime_config::self_heal_graphs_enabled()` latches
    /// permanently on first read, so co-scheduled tests in the same binary
    /// could otherwise observe (or poison) a value they never set.
    #[test]
    fn cell_self_heal_materializes_a_never_created_graph_on_first_query() {
        let _serial = crate::tauri_runtime::profile_env_serial()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let profile = std::env::temp_dir().join(format!(
            "garden-aduanera-selfheal-{}",
            uuid::Uuid::new_v4()
        ));
        std::env::set_var("GARDEN_PROFILE_DIR", &profile);
        std::env::set_var("GARDEN_SELF_HEAL_GRAPHS", "1");
        crate::runtime_config::init_self_heal_graphs_from_env();
        assert!(
            crate::runtime_config::self_heal_graphs_enabled(),
            "GARDEN_SELF_HEAL_GRAPHS=1 must latch true — if this trips, the \
             OnceLock was already initialized by a co-scheduled test; rerun \
             with -- --exact for this test alone"
        );

        // Reproduce a REAL owner-scoped cell's env, not the synthetic
        // CellGraphBoundary::for_test() shortcut — that helper bypasses
        // cell_registry_authority::verified_binding() entirely, which let an
        // earlier version of this test pass for the wrong reason (it never
        // populated the SAME PREFLIGHT OnceLock that self_heal_missing_graph's
        // materialization_event() reads independently). This is the
        // deterministic local-process seam `cell_registry_authority.rs`
        // documents as "used by the real E2E harness" —
        // GARDEN_CELL_REGISTRY_SNAPSHOT_JSON — so the DynamoDB read is never
        // reached and this stays a real-code, no-mocks test of the Rust
        // logic, not of AWS.
        let graph_id = "sirin-veedora-browser-lane-e2e";
        let owner = "user:e969f93e-40f1-70e6-92d3-a7907cd051e8";
        std::env::set_var("GARDEN_CELL_ID", "c-test-aduanera-diagnosis");
        std::env::set_var("GARDEN_CELL_OWNER", owner);
        std::env::set_var("GARDEN_CELL_GRAPH_ID", graph_id);
        std::env::set_var("GARDEN_CELL_GRAPH_GENERATION", "1");
        std::env::set_var("GARDEN_CELL_REGISTRY_REVISION", "1");
        std::env::set_var(
            "GARDEN_CELL_REGISTRY_SNAPSHOT_JSON",
            serde_json::json!({
                "owner": owner,
                "graphId": graph_id,
                "generation": 1,
                "lifecycleState": "provisioning",
                "registryRevision": 1,
            })
            .to_string(),
        );
        std::env::set_var(
            "GARDEN_CELL_LEASE_SECRET",
            "aduanera-diagnosis-test-secret-at-least-32-bytes-long",
        );

        let app = crate::tauri_runtime::build_mock_app_for_tests(false);
        let boundary = crate::cell_graph_boundary::CellGraphBoundary::from_process_env()
            .expect("a fully-populated owner-scoped env must build a boundary");
        assert!(
            boundary.is_enabled(),
            "owner-scoped env must produce an enabled (single-graph) boundary"
        );
        app.manage(Arc::new(boundary));

        // This is the exact call `dump_rdf_service`/`run_sparql_query_service`
        // make before touching the RDF store — the gateway has already scheduled
        // and started the cell (registration-only create, per routes.rs
        // create_graph_inner's Dynamo branch); this is its first request.
        let outcome = existing_graph_dir_reporting_heal(&app, graph_id);
        assert!(
            outcome.is_ok(),
            "F4c self-heal must materialize a registered-but-never-created \
             graph on first access, got {outcome:?}"
        );
        let (graph_dir, heal_outcome) = outcome.unwrap();
        assert_eq!(
            heal_outcome,
            GraphDirOutcome::SelfHealed,
            "first access to a graph nothing ever created must self-heal, not find it already existing"
        );
        assert!(
            graph_dir.join("graph.json").is_file(),
            "self-heal must leave a real graph.json behind, not just report success"
        );

        // A second access must see the now-real graph without healing again —
        // proves the heal is durable, not a per-call illusion.
        let second = existing_graph_dir_reporting_heal(&app, graph_id).expect("second lookup");
        assert_eq!(second.1, GraphDirOutcome::Existing);

        let _ = std::fs::remove_dir_all(&profile);
    }

    #[test]
    fn process_bound_authority_precedes_f4c_storage() {
        let app = crate::tauri_runtime::build_mock_app_for_tests(false);
        app.manage(Arc::new(
            crate::cell_graph_boundary::CellGraphBoundary::for_test(Some("graph-a")),
        ));

        assert!(authorize_cell_self_heal(&app, "graph-a").is_ok());
        assert_eq!(
            authorize_cell_self_heal(&app, "graph-b"),
            Err("graph not found: graph-b".to_string())
        );
        assert_eq!(
            materialization_event_for_lifecycle("provisioning"),
            "graph.initialize"
        );
        assert_eq!(
            materialization_event_for_lifecycle("active"),
            "graph.repair"
        );
        assert_eq!(
            materialization_event_for_lifecycle("repairing"),
            "graph.repair"
        );
    }
}

pub(crate) fn ensure_graph_index_dirs(graph_dir: &Path) -> Result<(), String> {
    create_dir_all(&graph_dir.join("indexes"))?;
    create_dir_all(&semantic_index_dir(graph_dir))?;
    Ok(())
}

pub(crate) fn ensure_graph_layout(graph_dir: &Path) -> Result<(), String> {
    ensure_graph_content_dirs(graph_dir)?;
    create_dir_all(&images_dir(graph_dir))?;
    ensure_graph_index_dirs(graph_dir)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_graph_dir(name: &str) -> PathBuf {
        let suffix = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        std::env::temp_dir().join(format!("mnemosyne-paths-{name}-{suffix}"))
    }

    #[test]
    fn ensure_graph_layout_creates_projection_and_index_directories() {
        let graph_dir = temp_graph_dir("layout");

        ensure_graph_layout(&graph_dir).expect("ensure graph layout");

        for relative in [
            "documents",
            "artifacts",
            "images",
            "ydocs/documents",
            "ydocs/workspace",
            "indexes",
            "indexes/semantic",
        ] {
            assert!(
                graph_dir.join(relative).is_dir(),
                "expected {relative} to exist"
            );
        }
        let _ = fs::remove_dir_all(graph_dir);
    }
}

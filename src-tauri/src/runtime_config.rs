use std::sync::OnceLock;

pub(crate) const PROFILE_ID: &str = "default";
pub(crate) const RUNTIME_PROFILE: &str = "local_only";
pub(crate) const LOCAL_GRAPH_ORIGIN: &str = "local";
pub(crate) const LOCAL_PROVIDER_ID: &str = "local-profile";
pub(crate) const MNEMO_NS: &str = "https://mnemosyne.local/ns#";
pub(crate) const MDOC_NS: &str = "http://mnemosyne.dev/doc#";
pub(crate) const WIRE_NS: &str = "http://mnemosyne.ai/vocab#";
pub(crate) const NFO_NS: &str = "http://www.semanticdesktop.org/ontologies/2007/03/22/nfo#";
pub(crate) const NIE_NS: &str = "http://www.semanticdesktop.org/ontologies/2007/01/19/nie#";
pub(crate) const DCTERMS_NS: &str = "http://purl.org/dc/terms/";
pub(crate) const RDF_TYPE: &str = "http://www.w3.org/1999/02/22-rdf-syntax-ns#type";
pub(crate) const XSD_NS: &str = "http://www.w3.org/2001/XMLSchema#";
pub(crate) const DOCUMENT_SCHEMA_VERSION: u32 = 1;
pub(crate) const YDOC_STATE_FILE: &str = "update-v1.bin";
pub(crate) const WORKSPACE_SNAPSHOT_FILE: &str = "workspace.json";
pub(crate) const SEMANTIC_INDEX_FILE: &str = "blocks.json";
pub(crate) const SEMANTIC_INDEX_REFRESH_JOB_TYPE: &str = "semantic_index_refresh";
pub(crate) const SEMANTIC_MODEL_PREPARE_JOB_TYPE: &str = "semantic_model_prepare";
pub(crate) const SEMANTIC_MODEL_CONFIG_FILE: &str = "selection.json";
pub(crate) const DOCLING_SETUP_FILE: &str = "runtime.json";
pub(crate) const PDF_PIPELINE_CONFIG_FILE: &str = "pipeline.json";
pub(crate) const LOOPBACK_MANIFEST_FILE: &str = "loopback.json";
pub(crate) const LOOPBACK_BIND_HOST: &str = "127.0.0.1";
pub(crate) const LOCAL_JOB_INLINE_RESULT_MAX_BYTES: usize = 256 * 1024;
pub(crate) const LOCAL_UPLOAD_MAX_BYTES: usize = 500 * 1024 * 1024;
pub(crate) const LOCAL_UPLOAD_REQUEST_MAX_BYTES: usize = LOCAL_UPLOAD_MAX_BYTES + 16 * 1024 * 1024;
pub(crate) const LOCAL_IMAGE_UPLOAD_MAX_BYTES: usize = 10 * 1024 * 1024;
/// Per-file cap for the Files rudiments (Vera, 2026-10-05: "a per-file upload
/// cap of 50 MB"), read as 50 MiB = 52,428,800 bytes: exactly this many bytes
/// is accepted, one more is refused. Enforced on every file-byte ingress the
/// Files contract names, in every mode, and on the parsing upload in cell mode.
pub(crate) const FILES_MAX_UPLOAD_BYTES: usize = 50 * 1024 * 1024;
pub(crate) const LOCAL_WEB_CLIP_MAX_BYTES: usize = 10 * 1024 * 1024;
pub(crate) const LOCAL_OPENAPI_JSON: &str = include_str!("../../parity/local-openapi.json");
pub(crate) const GRAPH_STATUS_ACTIVE: &str = "active";
pub(crate) const GRAPH_STATUS_DELETED: &str = "deleted";

/// The per-path memory-validation policy (the founder's EA-6 decision). A
/// Greenhouse knob, stored per-graph on [`crate::graph_record_store::GraphRecord`]
/// and (later) mapped to entitlement/tier defaults. Three modes:
///
/// - [`ValidationPolicy::Halt`] — REJECT a malformed write (loud-halt), returning
///   the structured SHACL violations to the caller (the agent) AS FEEDBACK so it
///   can repair and retry. This is the SAFE default: a legacy graph (no field in
///   `graph.json`) defaults here, so a broken write is user-visible, not silently
///   accepted.
/// - [`ValidationPolicy::FlagAndAccept`] — WRITE the memory anyway, but RECORD the
///   violation to the violation ledger (a Meaningful Object) for periodic review.
///   The flexible mode: the write lands, the violation is durable + queryable.
/// - [`ValidationPolicy::Off`] — legacy: NO validation runs (byte-identical to the
///   pre-EA-6 memory path).
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) enum ValidationPolicy {
    Halt,
    FlagAndAccept,
    Off,
}

impl ValidationPolicy {
    /// The stable string token used when this policy is materialized to RDF
    /// (the Greenhouse-readable face of the knob) and in human logs. Kept as the
    /// policy's canonical token API ahead of the Greenhouse projection that will
    /// surface it; not yet wired to a caller.
    #[allow(dead_code)]
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            ValidationPolicy::Halt => "halt",
            ValidationPolicy::FlagAndAccept => "flagAndAccept",
            ValidationPolicy::Off => "off",
        }
    }
}

impl Default for ValidationPolicy {
    fn default() -> Self {
        ValidationPolicy::Halt
    }
}

/// The serde default for a missing `validationPolicy` field on a legacy
/// `graph.json` — loud-halt (safe + agent-visible).
pub(crate) fn default_validation_policy() -> ValidationPolicy {
    ValidationPolicy::Halt
}

/// True when a policy equals the default — drives `skip_serializing_if` so a
/// legacy `graph.json` round-trips byte-for-byte (the field is only written when
/// it diverges from the default).
pub(crate) fn is_default_validation_policy(p: &ValidationPolicy) -> bool {
    *p == ValidationPolicy::default()
}

static SELF_HEAL_GRAPHS: OnceLock<bool> = OnceLock::new();

/// Read `GARDEN_SELF_HEAL_GRAPHS` once at startup (see
/// `loopback_server::start_loopback_server`) and cache it for the process's
/// lifetime — the "register once at startup" idiom already used by
/// `cell_durability::set_durable_dirs`. Gates F4c self-heal
/// (`graph_paths::self_heal_missing_graph`); default off. The gateway sets
/// this to `"1"` only in the env block it builds for cells it dynamically
/// spawns behind its own ACL (`cell_env` in `platform-next/gateway/src/cell.rs`)
/// — a bad `graph_id` there materializes a graph rather than 404ing. Bare
/// headless spawns (e.g. `sophia-mcp` local backend mode) never set it and
/// keep today's not-found behavior.
/// The single source of truth, read from the environment. Both the eager
/// initializer and the lazy accessor below go through this, so it cannot
/// matter which of them touches the OnceLock first.
fn self_heal_graphs_from_env() -> bool {
    std::env::var("GARDEN_SELF_HEAL_GRAPHS").as_deref() == Ok("1")
}

pub(crate) fn init_self_heal_graphs_from_env() {
    let _ = SELF_HEAL_GRAPHS.set(self_heal_graphs_from_env());
}

/// THE ORDER BUG THIS FIXES, kept because the failure was invisible and cost a
/// day. This accessor used to be `get_or_init(|| false)`. `get_or_init` is a
/// WRITE, not a read: whoever calls it first decides the value for the life of
/// the process. And on a cell whose graph has no `graph.json` yet, the first
/// caller is the boot-time warm-open, which runs BEFORE
/// `start_loopback_server` calls the initializer above:
///
///   gardend.rs:437  warm_open_cell_graph_store
///     -> existing_graph_dir -> existing_graph_dir_reporting_heal
///     -> graph.json missing -> self_heal_missing_graph
///     -> self_heal_graphs_enabled()   ... latches the OnceLock to FALSE
///   tauri_runtime.rs:221  start_loopback_server
///     -> init_self_heal_graphs_from_env()  ... set() returns Err, discarded
///
/// F4c self-heal was then dead for the rest of that cell's life, and dead
/// SILENTLY: `self_heal_missing_graph`'s disabled branch returns
/// `graph not found: {graph_id}` before it logs anything. Witnessed on Sirin
/// 2026-09-04/05 — every newly created graph was unreadable AND unwritable by
/// its own owner, through both creation doors, while all fourteen RESTORED
/// graphs were fine (their `graph.json` exists, so warm-open succeeds and
/// never touches this flag before the initializer does). The boot log shows
/// the order plainly: the warm-open warning precedes "Loopback API listening".
///
/// Reading the env in the initializer closure makes the flag order-independent
/// — the eager call is now an optimization, not a correctness requirement.
pub(crate) fn self_heal_graphs_enabled() -> bool {
    *SELF_HEAL_GRAPHS.get_or_init(self_heal_graphs_from_env)
}

#[cfg(test)]
mod self_heal_order_tests {
    //! The self-heal flag must not depend on who touches it first.
    //!
    //! A same-process test cannot prove this: `SELF_HEAL_GRAPHS` is a
    //! process-global `OnceLock`, and cargo runs unit tests as threads in ONE
    //! process, so any co-scheduled test that touches the flag first decides
    //! it for everybody. That is not a hypothetical — the F4c self-heal test
    //! in `graph_paths` carries a comment blaming exactly that for its own
    //! flakiness, which is how close this bug came to being noticed and
    //! wasn't. So this test re-executes the test binary as a CHILD PROCESS and
    //! reproduces the real gardend boot order inside it: READ the flag first
    //! (as boot-time warm-open does on a cell whose graph.json is missing),
    //! and only then call the initializer (as `start_loopback_server` does).
    //!
    //! Against `get_or_init(|| false)` the child's first assertion fails.

    const CHILD: &str = "GARDEN_SELF_HEAL_ORDER_CHILD";

    #[test]
    fn the_flag_survives_being_read_before_it_is_initialized() {
        if std::env::var(CHILD).is_ok() {
            // The production boot order, exactly: the read comes first.
            assert!(
                super::self_heal_graphs_enabled(),
                "reading the flag before init must still see GARDEN_SELF_HEAL_GRAPHS=1; \
                 a `get_or_init(|| false)` here latches the process to false forever \
                 and kills F4c self-heal silently"
            );
            super::init_self_heal_graphs_from_env();
            assert!(
                super::self_heal_graphs_enabled(),
                "the later initializer must not change the answer"
            );
            return;
        }

        let output = std::process::Command::new(
            std::env::current_exe().expect("test binary path"),
        )
        .args([
            "--exact",
            "runtime_config::self_heal_order_tests::the_flag_survives_being_read_before_it_is_initialized",
            "--nocapture",
        ])
        .env(CHILD, "1")
        .env("GARDEN_SELF_HEAL_GRAPHS", "1")
        .output()
        .expect("re-exec the test binary");

        assert!(
            output.status.success(),
            "child failed:\n--- stdout ---\n{}\n--- stderr ---\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
    }

    #[test]
    fn the_flag_is_false_when_the_variable_is_absent() {
        // Guard against a fix that simply returns true: with the variable
        // unset, a fresh process must still read false.
        if std::env::var(CHILD).is_ok() {
            assert!(!super::self_heal_graphs_enabled());
            return;
        }
        let output = std::process::Command::new(
            std::env::current_exe().expect("test binary path"),
        )
        .args([
            "--exact",
            "runtime_config::self_heal_order_tests::the_flag_is_false_when_the_variable_is_absent",
            "--nocapture",
        ])
        .env(CHILD, "1")
        .env_remove("GARDEN_SELF_HEAL_GRAPHS")
        .output()
        .expect("re-exec the test binary");
        assert!(
            output.status.success(),
            "child failed:\n{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
}

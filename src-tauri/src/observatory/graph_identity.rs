//! Pure identity for the `observatory` graph (§A.5). No I/O, no `Store`
//! access — see `authority_harness` for the store-backed proof surface and
//! `scripts/provision-observatory-graph.sh` for actually minting the graph
//! record via the existing local create-graph path.

use crate::rdf::graph_subject as rdf_graph_subject;

/// The one graph_id this whole campaign targets — matches every deployment
/// reference (`/g/observatory/…`, collector/poke naming, §A.5).
pub const GRAPH_ID: &str = "observatory";

/// `graph_subject("observatory")` = `urn:mnemosyne:local:graph:observatory`
/// (`ba1a97eccc83:src-tauri/src/rdf.rs:122-124`). Re-derived through the
/// crate's own `graph_subject`, never hand-rolled, so this can never drift
/// from what the authority gate actually reserves.
pub fn graph_subject() -> String {
    rdf_graph_subject(GRAPH_ID)
}

/// `urn:mnemosyne:local:graph:observatory:projection:obs:raw` — the bounded
/// 72h sliding-window bounded `CaptureEvent` graph: lifecycle/governance plus
/// the low-volume Hoja evidence index.
pub fn raw_graph_iri() -> String {
    format!("{}:projection:obs:raw", graph_subject())
}

/// `urn:mnemosyne:local:graph:observatory:projection:obs:rollups` — the
/// durable, subject-upserted projection graph (§A.2).
pub fn rollups_graph_iri() -> String {
    format!("{}:projection:obs:rollups", graph_subject())
}

/// `urn:mnemosyne:local:graph:observatory:projection:obs` — the un-suffixed
/// root. Not one of the two functional graphs Lane A writes, but it still
/// matches the reserved `:projection:` prefix and is worth proving reserved
/// in its own right (§A.6's "the bare base").
pub fn bare_projection_obs_graph_iri() -> String {
    format!("{}:projection:obs", graph_subject())
}

/// The three `:projection:obs*` IRIs the A0 smoke test proves reserved.
pub fn projection_obs_iris() -> [String; 3] {
    [
        raw_graph_iri(),
        rollups_graph_iri(),
        bare_projection_obs_graph_iri(),
    ]
}

/// `urn:mnemosyne:local:graph:observatory:user:rdf` — the human/agent
/// partition for the SAME graph_id. Used by the authority harness as the
/// negative control: proves a refusal is about reserved-ness specifically,
/// not a blanket rejection of every `GRAPH`/`WITH` clause or `load_rdf`
/// call.
pub fn user_rdf_graph_iri() -> String {
    format!("{}:user:rdf", graph_subject())
}

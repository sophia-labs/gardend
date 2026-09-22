use crate::rdf::graph_subject;

pub(crate) fn graph_projection_graph_iri(graph_id: &str) -> String {
    format!("{}:projection:graph", graph_subject(graph_id))
}

pub(crate) fn workspace_projection_graph_iri(graph_id: &str) -> String {
    format!("{}:projection:workspace", graph_subject(graph_id))
}

pub(crate) fn document_projection_graph_iri(graph_id: &str, document_id: &str) -> String {
    format!(
        "{}:projection:document:{document_id}",
        graph_subject(graph_id)
    )
}

pub(crate) fn user_rdf_graph_iri(graph_id: &str) -> String {
    format!("{}:user:rdf", graph_subject(graph_id))
}

/// The reserved, materializer-only memory projection graph for a graph_id —
/// `urn:mnemosyne:local:graph:{id}:projection:memory`. Authoritative typed memory
/// (the `mem:` core pack) materializes here DIRECT-ON-STORE; the user:rdf SPARQL
/// service must keep REFUSING it (it is already caught by `is_reserved_rdf_graph_iri`
/// and `contains_reserved_named_graph_target` via the `:projection:` prefix —
/// which is exactly what we want; memory writes bypass that service). NO
/// authority-gate edits are needed or wanted here.
pub(crate) fn memory_projection_graph_iri(graph_id: &str) -> String {
    format!("{}:projection:memory", graph_subject(graph_id))
}

/// The reserved, materializer-only VIOLATION LEDGER projection graph for a
/// graph_id — `urn:mnemosyne:local:graph:{id}:projection:violations` (EA-6). SHACL
/// validation failures recorded under the FlagAndAccept policy materialize here
/// DIRECT-ON-STORE as observer-relative RDF testimony (a Meaningful Object:
/// recorded testimony ABOUT testimony). Like every other `:projection:*` graph it
/// is reserved (caught by `is_reserved_rdf_graph_iri` /
/// `contains_reserved_named_graph_target` via the `:projection:` prefix), so the
/// user:rdf SPARQL service refuses it — the ledger is materializer-owned, never
/// user-writable, and is REGENERABLE from the validation events (a projection,
/// never a source of truth for the write decision).
pub(crate) fn violations_projection_graph_iri(graph_id: &str) -> String {
    format!("{}:projection:violations", graph_subject(graph_id))
}

/// The reserved, materializer-only KG-ULTRA projection graph for a graph_id —
/// `urn:mnemosyne:local:graph:{id}:projection:kg-ultra`. Durable KG-ULTRA
/// "intuition" records materialize here as Meaningful Objects: model-attributed,
/// defeasible structural suggestions over a source graph snapshot. The service's
/// private tensors / integerized ULTRA graph are NOT stored here; only inspectable
/// testimony that an agent or user might cite, accept, reject, or evaluate lands
/// in this projection.
pub(crate) fn kg_ultra_projection_graph_iri(graph_id: &str) -> String {
    format!("{}:projection:kg-ultra", graph_subject(graph_id))
}

/// The reserved, materializer-only SEMMA+ semantic projection graph for a graph_id —
/// `urn:mnemosyne:local:graph:{id}:projection:semantic`. The semantic scaffold
/// crystallizes here as defeasible, derived testimony over the vector plane:
/// entity refs, neighbor edges, clusters, and memberships. Vectors themselves
/// remain in the filesystem (`indexes/semantic/*`), never RDF literals.
pub(crate) fn semantic_projection_graph_iri(graph_id: &str) -> String {
    format!("{}:projection:semantic", graph_subject(graph_id))
}

/// The reserved, materializer-owned LongMemEval labeled-memory projection graph
/// for a graph_id — `urn:mnemosyne:local:graph:{id}:projection:lme-labeled-memory`.
/// This is the durable frame-first calibration surface: source spans, concept
/// cues, observations, frames, derivations, adjudications, and backprojection
/// checks. It is the Garden-facing object that KG-ULTRA can later consume; gold
/// answer-session ids are evaluation labels only.
pub(crate) fn lme_labeled_memory_projection_graph_iri(graph_id: &str) -> String {
    format!("{}:projection:lme-labeled-memory", graph_subject(graph_id))
}

/// The per-OBSERVER memory projection graph — Variant B (per-observer named graph).
/// When `observer` is non-empty, route this witness's memory into a perspective
/// graph `…:projection:memory:agent:{observer}`; when empty/absent, fall back to
/// the SHARED commons graph (`memory_projection_graph_iri`, today's behavior,
/// byte-identical). The `:agent:{id}` segment is a PREFIX a future
/// `:agent:{id}:persona:{pid}` segment can extend (HIVE-MIND forward-compat — we
/// do not preclude personas, just don't build them). Still under `:projection:*`,
/// so it stays reserved (caught by `is_reserved_rdf_graph_iri`) with ZERO
/// authority-gate edits.
pub(crate) fn memory_projection_graph_iri_for(graph_id: &str, observer: &str) -> String {
    let base = memory_projection_graph_iri(graph_id);
    match observer_segment(observer) {
        Some(seg) => format!("{base}:agent:{seg}"),
        None => base,
    }
}

/// The reserved seed-marker bookkeeping graph for a graph_id —
/// `urn:mnemosyne:local:graph:{id}:projection:seed`. Holds exactly one quad:
/// `<graph_subject> mnemo:seedKey "{seed_key}"`, written by
/// `rdf_seed_service::ensure_graph_store_seeded` after a completed
/// materialization pass. Living INSIDE the Oxigraph store is the point: the
/// durable-plane flush snapshots store+marker atomically (`Store::backup`),
/// so a restored/rehydrated cell reads back exactly the key describing the
/// materialization state the restored store actually contains — the previous
/// process-local-only marker was reset by every restart, making the first
/// SPARQL query on a fresh pod wholesale re-materialize every document.
/// Reserved via the `:projection:` prefix (caught by
/// `is_reserved_rdf_graph_iri`), so user SPARQL updates and dataset imports
/// can never forge or clobber it — no authority-gate edits needed.
pub(crate) fn seed_marker_graph_iri(graph_id: &str) -> String {
    format!("{}:projection:seed", graph_subject(graph_id))
}

/// The reserved, materializer-only salience projection graph for a graph_id —
/// `urn:mnemosyne:local:graph:{id}:projection:salience`. The salience
/// `BlockValuation` class span reconciles here DIRECT-ON-STORE; like every other
/// `:projection:*` graph it is reserved (caught by `is_reserved_rdf_graph_iri` /
/// `contains_reserved_named_graph_target` via the `:projection:` prefix), so the
/// user:rdf service refuses it. Replaces the historical default-graph WART.
pub(crate) fn salience_projection_graph_iri(graph_id: &str) -> String {
    format!("{}:projection:salience", graph_subject(graph_id))
}

/// The reserved, materializer-only Mithras Flow board projection lane for a
/// graph_id — `urn:mnemosyne:local:graph:{id}:projection:flow` (interfaces.md
/// §A; FLOW-GS-3). The board's `resource` root reconciles here DIRECT-ON-STORE
/// (`flow_board_reconcile`, unit G3) and the registered `flow` Emporium pack's
/// `write_target: projection:flow` (unit G1) resolves to the same IRI — one
/// lane, two sanctioned materializer doors, both value-diffed. The room is the
/// only authority (GS-3: "its RDF form is a declared projection, never an
/// independent authority"): like every `:projection:*` graph this IRI is
/// reserved (caught by `is_reserved_rdf_graph_iri` /
/// `contains_reserved_named_graph_target` via the `:projection:` prefix), so a
/// user `sparql_update`/`rdf_load` targeting it is refused with ZERO
/// authority-gate edits — GS-3's acceptance rides the prefix rule, and
/// `flow_board_reconcile::tests` proves it stays that way.
pub(crate) fn flow_projection_graph_iri(graph_id: &str) -> String {
    format!("{}:projection:flow", graph_subject(graph_id))
}

/// The per-OBSERVER salience projection graph — the Valuation analog of
/// [`song_projection_graph_iri_for`] / [`memory_projection_graph_iri_for`]. Salience
/// is the ONE faculty that was NOT witness-scoped: every observer's valuations
/// collapsed into the single `:projection:salience` store + graph and SUMMED into one
/// global `LocalBlockValueRecord` — "a view from nowhere" the observer-relative
/// ontology forbids. A non-empty `observer` routes this witness's valuations into
/// `…:projection:salience:agent:{observer}` so two witnesses' valuations of the SAME
/// block reconcile into DISTINCT named graphs and never sum; empty/absent ⇒ the shared
/// commons graph (today's singleton, byte-identical). Reserved via the `:projection:`
/// prefix (caught by `is_reserved_rdf_graph_iri` — no authority-gate edits).
pub(crate) fn salience_projection_graph_iri_for(graph_id: &str, observer: &str) -> String {
    let base = salience_projection_graph_iri(graph_id);
    match observer_segment(observer) {
        Some(seg) => format!("{base}:agent:{seg}"),
        None => base,
    }
}

/// The reserved, materializer-only song projection graph for a graph_id —
/// `urn:mnemosyne:local:graph:{id}:projection:song`. The Song / SongVerse /
/// SongCoda class spans reconcile here DIRECT-ON-STORE; reserved via the
/// `:projection:` prefix. Replaces the historical default-graph WART (inherited
/// from salience).
pub(crate) fn song_projection_graph_iri(graph_id: &str) -> String {
    format!("{}:projection:song", graph_subject(graph_id))
}

/// The per-OBSERVER song projection graph — the Song analog of
/// [`memory_projection_graph_iri_for`]. A non-empty `observer` routes the Song
/// into `…:projection:song:agent:{observer}` so the GRAPH-scoped DELETE in the
/// song reconcile path (`geist_song_rdf`) can never reach a co-tenant witness's
/// Song; empty/absent = the shared commons graph (today's singleton, byte-
/// identical). Reserved via the `:projection:` prefix (no gate edits).
pub(crate) fn song_projection_graph_iri_for(graph_id: &str, observer: &str) -> String {
    let base = song_projection_graph_iri(graph_id);
    match observer_segment(observer) {
        Some(seg) => format!("{base}:agent:{seg}"),
        None => base,
    }
}

/// The canonical absolute-IRI FORM of a wire-supplied observer id — used as the
/// RDF object of `mem:observedBy` and `prov:wasAttributedTo`. A full IRI (anything
/// with a scheme `…://…` or a `urn:` prefix) passes through verbatim; a BARE token
/// (e.g. `agent-1a2b…`) is given a stable absolute IRI under the agent URN space
/// `urn:sophia:agent:{token}` so the object is a VALID IRI (a bare token is NOT a
/// valid absolute IRI and would otherwise fall back to the invalid-uri sentinel).
/// This is SERIALIZING (a canonical IRI form), NOT minting a new identity. Returns
/// `None` for the empty/absent commons case.
pub(crate) fn observer_iri(observer: &str) -> Option<String> {
    let trimmed = observer.trim();
    if trimmed.is_empty() {
        return None;
    }
    if trimmed.contains("://") || trimmed.starts_with("urn:") {
        return Some(trimmed.to_string());
    }
    // A bare token → the agent URN space. Reuse the segment sanitizer so the local
    // part is URN-safe.
    let seg = observer_segment(trimmed).unwrap_or_else(|| trimmed.to_string());
    Some(format!("urn:sophia:agent:{seg}"))
}

/// Normalize a wire-supplied observer id into a single, IRI-safe graph segment, or
/// `None` for the empty/absent commons case (the BYTE-IDENTITY contract: an empty
/// observer reproduces today's graph IRI exactly).
///
/// L0 TRUSTS but SERIALIZES the observer (it does not mint it). The id may be a
/// bare `agent-<hex>` or a full IRI (e.g. `urn:…:agent:gamma`); we percent-ish
/// sanitize any character that would break the IRI or the `:agent:` path
/// structure (whitespace, `<>"{}|\^` and the `:`/`/` path separators) into `_`,
/// keeping the segment a single, terminal-AGNOSTIC token (a persona segment can
/// still be appended after it by the caller).
pub(crate) fn observer_segment(observer: &str) -> Option<String> {
    let trimmed = observer.trim();
    if trimmed.is_empty() {
        return None;
    }
    let sanitized: String = trimmed
        .chars()
        .map(|c| {
            if c.is_whitespace()
                || matches!(
                    c,
                    ':' | '/' | '<' | '>' | '"' | '{' | '}' | '|' | '\\' | '^' | '`'
                )
            {
                '_'
            } else {
                c
            }
        })
        .collect();
    Some(sanitized)
}

pub(crate) fn user_rdf_target_graph_iri(
    graph_id: &str,
    requested_graph_iri: Option<&str>,
) -> Result<String, String> {
    let target = requested_graph_iri
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| user_rdf_graph_iri(graph_id));
    if is_reserved_rdf_graph_iri(graph_id, &target) {
        return Err(format!(
            "RDF writes cannot target reserved projection graph {target}"
        ));
    }
    Ok(target)
}

pub(crate) fn validate_sparql_update_authority(graph_id: &str, update: &str) -> Result<(), String> {
    // Authority decisions must inspect SPARQL structure, not user data carried
    // inside a string literal. Layout documents legitimately embed read-only
    // SPARQL such as `GRAPH <...:projection:...>` in ux:layoutJson; scanning the
    // raw update mistakes that literal text for a write target and rejects the
    // otherwise-admitted :ux:config update. Comments are non-structural too.
    //
    // Keep IRIREFs intact: unlike the query-warning tokenizer, this gate needs
    // to inspect the actual `<...>` following a real GRAPH/WITH keyword.
    let structure = strip_sparql_strings_and_comments(update);
    let upper = structure.to_ascii_uppercase();
    for forbidden in ["CLEAR ALL", "CLEAR NAMED", "DROP ALL", "DROP NAMED"] {
        if upper.contains(forbidden) {
            return Err(format!(
                "SPARQL update operation {forbidden} is not allowed in local RDF authority mode"
            ));
        }
    }
    if upper.contains("GRAPH ?") || upper.contains("WITH ?") {
        return Err(
            "SPARQL updates with variable GRAPH/WITH targets are not allowed in local RDF authority mode"
                .to_string(),
        );
    }

    if contains_reserved_named_graph_target(graph_id, &structure) {
        return Err("SPARQL update targets a reserved local RDF authority graph".to_string());
    }
    Ok(())
}

/// Remove SPARQL string literals and comments while preserving all structural
/// tokens and IRIREFs. This is deliberately a small lexer rather than a regex:
/// it handles single/double and triple-quoted strings, escaped quote characters,
/// and `#` inside `<iri#fragments>` without treating the fragment as a comment.
fn strip_sparql_strings_and_comments(update: &str) -> String {
    let chars: Vec<char> = update.chars().collect();
    let mut out = String::with_capacity(chars.len());
    let mut i = 0;
    while i < chars.len() {
        match chars[i] {
            '#' => {
                while i < chars.len() && chars[i] != '\n' {
                    i += 1;
                }
            }
            '<' => {
                // Preserve the full IRIREF, including any `#` fragment, because
                // GRAPH/WITH target validation consumes it after this pass.
                while i < chars.len() {
                    let ch = chars[i];
                    out.push(ch);
                    i += 1;
                    if ch == '>' {
                        break;
                    }
                    if ch == '\\' && i < chars.len() {
                        out.push(chars[i]);
                        i += 1;
                    }
                }
            }
            quote @ ('"' | '\'') => {
                let triple = i + 2 < chars.len() && chars[i + 1] == quote && chars[i + 2] == quote;
                let delimiter_len = if triple { 3 } else { 1 };
                out.push(' ');
                i += delimiter_len;
                loop {
                    if i >= chars.len() {
                        break;
                    }
                    if chars[i] == '\\' && i + 1 < chars.len() {
                        i += 2;
                        continue;
                    }
                    if triple {
                        if i + 2 < chars.len()
                            && chars[i] == quote
                            && chars[i + 1] == quote
                            && chars[i + 2] == quote
                        {
                            i += 3;
                            break;
                        }
                    } else if chars[i] == quote {
                        i += 1;
                        break;
                    }
                    i += 1;
                }
            }
            ch => {
                out.push(ch);
                i += 1;
            }
        }
    }
    out
}

fn contains_reserved_named_graph_target(graph_id: &str, update: &str) -> bool {
    let graph_root = graph_subject(graph_id);
    let projection_prefix = format!("{graph_root}:projection:");
    for keyword in ["GRAPH", "Graph", "graph", "WITH", "With", "with"] {
        if update.contains(&format!("{keyword} <{graph_root}>"))
            || update.contains(&format!("{keyword} <{projection_prefix}"))
            || update.contains(&format!("{keyword} <urn:mnemosyne:local:profile:"))
        {
            return true;
        }
    }
    false
}

/// `pub(crate)` (not module-private) so `rdf_query_service::load_rdf_dataset_into_store`
/// (A9) can run the IDENTICAL reserved-graph check against every named graph
/// a dataset-format payload (TriG/N-Quads/JSON-LD) carries INLINE, not just
/// the single explicit target `load_rdf`/`run_sparql_update` are given
/// up front. No logic changes here — this is the same predicate the SPARQL
/// update gate and `load_rdf` target-resolution already use.
pub(crate) fn is_reserved_rdf_graph_iri(graph_id: &str, graph_iri: &str) -> bool {
    let graph_root = graph_subject(graph_id);
    graph_iri == graph_root
        || graph_iri.starts_with(&format!("{graph_root}:projection:"))
        || graph_iri.starts_with("urn:mnemosyne:local:profile:")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn user_rdf_target_defaults_to_user_graph_and_rejects_projection_graphs() {
        assert_eq!(
            user_rdf_target_graph_iri("graph-a", None).expect("default user graph"),
            "urn:mnemosyne:local:graph:graph-a:user:rdf"
        );
        assert_eq!(
            user_rdf_target_graph_iri("graph-a", Some(" urn:custom:user ")).expect("custom graph"),
            "urn:custom:user"
        );
        assert!(user_rdf_target_graph_iri(
            "graph-a",
            Some("urn:mnemosyne:local:graph:graph-a:projection:workspace")
        )
        .is_err());
    }

    #[test]
    fn sparql_update_authority_rejects_reserved_and_broad_targets() {
        assert!(validate_sparql_update_authority(
            "graph-a",
            "INSERT DATA { <urn:s> <urn:p> <urn:o> }"
        )
        .is_ok());
        assert!(validate_sparql_update_authority("graph-a", "CLEAR ALL").is_err());
        assert!(validate_sparql_update_authority(
            "graph-a",
            "DELETE { GRAPH ?g { ?s ?p ?o } } WHERE { GRAPH ?g { ?s ?p ?o } }"
        )
        .is_err());
        assert!(validate_sparql_update_authority(
            "graph-a",
            "INSERT DATA { GRAPH <urn:mnemosyne:local:graph:graph-a:projection:workspace> { <urn:s> <urn:p> <urn:o> } }"
        )
        .is_err());
        assert!(validate_sparql_update_authority(
            "graph-a",
            "INSERT DATA { <urn:mnemosyne:local:graph:graph-a:projection:workspace> <urn:p> <urn:o> }"
        )
        .is_ok());
    }

    #[test]
    fn memory_projection_graph_is_reserved() {
        // The memory projection graph MUST be reserved (caught by the :projection:
        // prefix) so the user:rdf SPARQL service keeps refusing it — memory writes
        // go direct-on-store, bypassing that service. This pins the "no gate edits"
        // invariant: loosening the gate would break this and is forbidden.
        let g = "lab";
        let mem = memory_projection_graph_iri(g);
        assert_eq!(mem, "urn:mnemosyne:local:graph:lab:projection:memory");
        assert!(is_reserved_rdf_graph_iri(g, &mem));
        // And the user:rdf service rejects an attempt to GRAPH-target it.
        assert!(user_rdf_target_graph_iri(g, Some(&mem)).is_err());
        let wrapped = format!("INSERT DATA {{ GRAPH <{mem}> {{ <urn:s> <urn:p> <urn:o> }} }}");
        assert!(validate_sparql_update_authority(g, &wrapped).is_err());
    }

    #[test]
    fn per_observer_memory_and_song_graphs_are_reserved_and_empty_is_byte_identical() {
        let g = "lab";

        // Empty observer = the shared commons graph, byte-for-byte today's IRI.
        assert_eq!(
            memory_projection_graph_iri_for(g, ""),
            memory_projection_graph_iri(g)
        );
        assert_eq!(
            song_projection_graph_iri_for(g, "   "),
            song_projection_graph_iri(g)
        );

        // Non-empty observer = a `:agent:{id}` perspective graph, still RESERVED.
        let mem_a = memory_projection_graph_iri_for(g, "agent-abc123");
        assert_eq!(
            mem_a,
            "urn:mnemosyne:local:graph:lab:projection:memory:agent:agent-abc123"
        );
        assert!(is_reserved_rdf_graph_iri(g, &mem_a));
        let song_a = song_projection_graph_iri_for(g, "agent-abc123");
        assert_eq!(
            song_a,
            "urn:mnemosyne:local:graph:lab:projection:song:agent:agent-abc123"
        );
        assert!(is_reserved_rdf_graph_iri(g, &song_a));

        // Distinct observers ⇒ distinct graphs (isolation precondition).
        assert_ne!(
            memory_projection_graph_iri_for(g, "agent-aaa"),
            memory_projection_graph_iri_for(g, "agent-bbb")
        );

        // A full-IRI observer is sanitized into ONE path segment (`:`/`/` → `_`),
        // so the `:agent:` structure is preserved (the leaf is the observer).
        let mem_iri_obs = memory_projection_graph_iri_for(g, "urn:x:agent:gamma");
        assert_eq!(
            mem_iri_obs,
            "urn:mnemosyne:local:graph:lab:projection:memory:agent:urn_x_agent_gamma"
        );
        assert!(is_reserved_rdf_graph_iri(g, &mem_iri_obs));

        // HIVE-MIND forward-compat: the `:agent:{id}` IRI is a PREFIX a future
        // persona segment can extend, and the extension stays reserved.
        let persona = format!("{mem_a}:persona:p1");
        assert!(persona.starts_with(&mem_a));
        assert!(is_reserved_rdf_graph_iri(g, &persona));

        // The user:rdf service still refuses an attempt to GRAPH-target the
        // per-observer graph (the no-gate-edits invariant holds for the new IRIs).
        assert!(user_rdf_target_graph_iri(g, Some(&mem_a)).is_err());
        let wrapped = format!("INSERT DATA {{ GRAPH <{mem_a}> {{ <urn:s> <urn:p> <urn:o> }} }}");
        assert!(validate_sparql_update_authority(g, &wrapped).is_err());
    }

    #[test]
    fn ux_config_seed_form_passes_authority() {
        // WP0.4 — the idempotent UX-subgraph seed must pass the authority gate.
        // It targets the NON-reserved `:ux:config` graph (parallels `:user:rdf`,
        // not `:projection:`) by LITERAL IRI, with an INSERT … WHERE FILTER NOT
        // EXISTS idempotent guard. The existing tests only cover INSERT DATA; this
        // pins the WHERE-guard form (the seed's actual shape).
        let graph_id = "g-abc";
        let ux_graph = format!("urn:mnemosyne:local:graph:{graph_id}:ux:config");

        // `:ux:config` is not a reserved RDF authority graph.
        assert!(!is_reserved_rdf_graph_iri(graph_id, &ux_graph));

        // The seed form (literal GRAPH, idempotent WHERE-guard) is accepted.
        let seed = format!(
            "INSERT {{ GRAPH <{ux_graph}> {{ \
             <http://sophia.ai/ux#GardenDefault> a <http://sophia.ai/ux#Workspace> }} }} \
             WHERE {{ FILTER NOT EXISTS {{ GRAPH <{ux_graph}> {{ ?s ?p ?o }} }} }}"
        );
        assert!(validate_sparql_update_authority(graph_id, &seed).is_ok());

        // A VARIABLE-GRAPH guard would be rejected — proving the seed MUST use the
        // literal-GRAPH form (the WP0.4 / WP5.1 constraint).
        let bad = format!(
            "INSERT {{ GRAPH <{ux_graph}> {{ ?s ?p ?o }} }} \
             WHERE {{ GRAPH ?g {{ ?s ?p ?o }} }}"
        );
        assert!(validate_sparql_update_authority(graph_id, &bad).is_err());

        // And writes to the reserved projection graph are still refused.
        assert!(user_rdf_target_graph_iri(graph_id, Some(&ux_graph)).is_ok());
    }

    #[test]
    fn durable_domain_source_ledgers_are_user_writeable_but_projections_are_not() {
        let graph_id = "phanes";
        let source_graph = "urn:mnemosyne:local:graph:phanes:source:discord";
        let projection_graph = "urn:mnemosyne:local:graph:phanes:projection:domain-manifest";

        assert!(!is_reserved_rdf_graph_iri(graph_id, source_graph));
        assert!(user_rdf_target_graph_iri(graph_id, Some(source_graph)).is_ok());
        assert!(validate_sparql_update_authority(
            graph_id,
            &format!(
                "INSERT DATA {{ GRAPH <{source_graph}> {{ <urn:event> <urn:kind> <urn:source> }} }}"
            )
        )
        .is_ok());

        assert!(is_reserved_rdf_graph_iri(graph_id, projection_graph));
        assert!(user_rdf_target_graph_iri(graph_id, Some(projection_graph)).is_err());
    }

    #[test]
    fn ux_layout_json_may_contain_read_only_projection_queries() {
        let graph_id = "observatory";
        let ux_graph = format!("urn:mnemosyne:local:graph:{graph_id}:ux:config");
        let projection_graph =
            format!("urn:mnemosyne:local:graph:{graph_id}:projection:obs:rollups");
        let update = format!(
            "PREFIX ux: <http://mnemosyne.dev/ux#>\n\
             DELETE {{ GRAPH <{ux_graph}> {{ <urn:surface> ux:layoutJson ?old }} }}\n\
             INSERT {{ GRAPH <{ux_graph}> {{ <urn:surface> ux:layoutJson \
             \"\"\"{{\\\"queryId\\\":\\\"SELECT * WHERE {{ GRAPH <{projection_graph}> {{ ?s ?p ?o }} }}\\\"}}\"\"\" }} }}\n\
             WHERE {{ OPTIONAL {{ GRAPH <{ux_graph}> {{ <urn:surface> ux:layoutJson ?old }} }} }}"
        );

        assert!(validate_sparql_update_authority(graph_id, &update).is_ok());
        assert!(!strip_sparql_strings_and_comments(&update).contains(&projection_graph));
    }

    #[test]
    fn literal_and_comment_text_cannot_trigger_or_hide_reserved_target_checks() {
        let graph_id = "graph-a";
        let projection = format!("urn:mnemosyne:local:graph:{graph_id}:projection:workspace");
        let user_graph = format!("urn:mnemosyne:local:graph:{graph_id}:user:rdf");

        let harmless = format!(
            "INSERT DATA {{ GRAPH <{user_graph}> {{ <urn:s> <urn:p> \
             \"GRAPH <{projection}> CLEAR ALL\" }} }} # WITH <{projection}>"
        );
        assert!(validate_sparql_update_authority(graph_id, &harmless).is_ok());

        let forbidden =
            format!("INSERT DATA {{ GRAPH <{projection}> {{ <urn:s> <urn:p> <urn:o> }} }}");
        assert!(validate_sparql_update_authority(graph_id, &forbidden).is_err());
    }
}

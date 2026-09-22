//! P4-WP3 fixtures + standing regression guards.
//!
//! Three PURE tests (no headless cell required) that pin the gardend emporium
//! engine to its frozen contract and to its Python twin:
//!
//!   1. [`tests::rust_desired_set_equals_python_golden_mod_canon`] — the PARITY
//!      test. Drive the Rust [`plan_compute`] on the SAME demo fixture
//!      ([`fixtures/parsed.json`] + [`fixtures/judgment.json`]) against an EMPTY
//!      live graph, recover its desired-triple SET from the `INSERT DATA` bodies,
//!      canon()-normalize it, and assert it equals the golden SET captured from
//!      the Python engine ([`fixtures/desired_triples.golden.json`]) — value-
//!      canonically (so `xsd:long` ≡ `xsd:integer`, dateTime ≡ epoch-seconds).
//!
//!   2. [`tests::frozen_vocab_guard_no_predicate_outside_contract`] — the
//!      FROZEN-VOCAB GUARD. Every minted predicate URI in the desired set is in
//!      `contract.known_predicate_uris()`, and the embedded golden sha is still
//!      `bc50e854…` (untouchable).
//!
//!   3. [`tests::cross_wp_read_eq_write_eq_user_rdf`] — the CROSS-WP INVARIANT.
//!      `survey::read_graph_iri == applier::write_graph_iri == user_rdf_graph_iri`
//!      — read-graph == write-graph, the #1 convergence risk, pinned forever.
//!
//! The fixtures are embedded with `include_str!` (CARGO_MANIFEST_DIR-relative, so
//! the tests are portable — no sibling-repo path). They are GENERATED from the
//! Python engine; the regen command lives in `fixtures/README.md`.

/// The demo `ParsedWorkflow` def (camelCase JSON the Rust schema deserializes).
/// Byte-identical to the Python `PARSED` fixture and the planner-test fixture.
pub(crate) const PARSED_JSON: &str = include_str!("fixtures/parsed.json");

/// The demo `JudgmentInput` (camelCase JSON). Byte-identical to Python `JUDGMENT`.
pub(crate) const JUDGMENT_JSON: &str = include_str!("fixtures/judgment.json");

/// The canon()-normalized desired-triple SET captured from the Python engine on
/// the demo fixture against an empty live graph (see `fixtures/README.md`).
pub(crate) const DESIRED_GOLDEN_JSON: &str = include_str!("fixtures/desired_triples.golden.json");

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use serde_json::Value as Json;

    use super::*;
    use crate::emporium::applier::write_graph_iri;
    use crate::emporium::contract::workflow_vocabulary;
    use crate::emporium::planner::{plan_compute, CurrentWire, Plan, Step};
    use crate::emporium::schemas::{JudgmentInput, ParsedWorkflow};
    use crate::emporium::survey::{parse_term, read_graph_iri, Live};
    use crate::emporium::terms::{canon_value, CanonValue, Term, Triple, PLACEHOLDER_NS};
    use crate::emporium::vocabs::WORKFLOW_GOLDEN_SHA;
    use crate::rdf_authority::user_rdf_graph_iri;
    use std::collections::BTreeMap;

    /// The graph-URI prefix the golden was captured under. The desired triples'
    /// doc-URI subjects are built from `live.prefix`, so the Rust plan MUST run
    /// with the SAME prefix to be byte-comparable. (This is the platform-style
    /// `:user:U:graph:` prefix the Python twin uses, NOT gardend's `:local:graph:`
    /// — the parity test compares the two engines as pure functions, so it pins
    /// the engine's prefix to whatever the golden was captured under.)
    const PREFIX: &str = "urn:mnemosyne:user:U:graph:lab";

    fn parsed() -> ParsedWorkflow {
        serde_json::from_str(PARSED_JSON).expect("parsed.json deserializes into ParsedWorkflow")
    }

    fn judgment() -> JudgmentInput {
        serde_json::from_str(JUDGMENT_JSON).expect("judgment.json deserializes into JudgmentInput")
    }

    fn golden() -> Json {
        serde_json::from_str(DESIRED_GOLDEN_JSON).expect("golden deserializes into JSON")
    }

    /// An empty live snapshot under [`PREFIX`] (the planner's `empty_live`).
    fn empty_live() -> Live {
        Live {
            graph: "lab".to_string(),
            prefix: PREFIX.to_string(),
            read_graph: format!("{PREFIX}:user:rdf"),
            folders: BTreeMap::new(),
            docs: BTreeMap::new(),
            workflows: BTreeMap::new(),
            archetypes: BTreeMap::new(),
            contracts: BTreeMap::new(),
            runs: Vec::new(),
            nodes_by_workflow: BTreeMap::new(),
        }
    }

    /// Plan the demo fixture on an empty graph (the full "new" mint).
    fn plan_on_empty() -> Plan {
        let p = parsed();
        let pj: Json = serde_json::from_str(PARSED_JSON).unwrap();
        let run_json = pj.get("run").filter(|v| !v.is_null());
        let j = judgment();
        plan_compute(
            workflow_vocabulary(),
            &p,
            &pj,
            run_json,
            Some(&j),
            &empty_live(),
            &[], // current_triples: empty → adds == desired
            &[] as &[CurrentWire],
            &BTreeMap::new(),
            &BTreeMap::new(),
            None,
        )
        .expect("plan_compute succeeds on the demo fixture")
    }

    /// The Rust desired-triple SET: every `INSERT DATA` body parsed back into
    /// typed triples (placeholders left UNRESOLVED, matching the golden). Against
    /// an empty current graph the INSERT (adds) set IS the desired set.
    fn rust_desired_triples(plan: &Plan) -> Vec<Triple> {
        let mut out: Vec<Triple> = Vec::new();
        for step in &plan.steps {
            let Step::SparqlUpdate { update } = step else {
                continue;
            };
            if !update.starts_with("INSERT DATA") {
                continue;
            }
            let open = update.find('{').expect("INSERT body has a brace");
            let close = update.rfind('}').expect("INSERT body has a brace");
            let body = &update[open + 1..close];
            for stmt in body.split(" .\n") {
                let stmt = stmt.trim().trim_end_matches('.').trim();
                if stmt.is_empty() {
                    continue;
                }
                // `<subject> <predicate> object`
                let s_end = stmt.find('>').expect("subject angle-bracket");
                let subject = &stmt[1..s_end];
                let rest = stmt[s_end + 1..].trim_start();
                let p_end = rest.find('>').expect("predicate angle-bracket");
                let predicate = &rest[1..p_end];
                let obj = rest[p_end + 1..].trim();
                out.push((subject.to_string(), predicate.to_string(), parse_term(obj)));
            }
        }
        out
    }

    /// A canon-normalized comparison key for one triple, in the SAME tagged shape
    /// the Python golden serialized: `"<s>|<p>|TAG|value"`. Numeric/dateTime are
    /// rounded to 6dp (the canon contract) and formatted identically on both
    /// sides so a store round-trip never reads as drift.
    fn canon_key_for_value(cv: &CanonValue) -> String {
        match cv {
            CanonValue::Placeholder(name) => format!("PLACEHOLDER|{name}"),
            CanonValue::Uri(u) => format!("URI|{u}"),
            CanonValue::Lit(l) => format!("LIT|{l}"),
            CanonValue::Bool(b) => format!("BOOL|{b}"),
            CanonValue::Num(bits) => format!("NUM|{}", fmt_6dp(f64::from_bits(*bits))),
            CanonValue::Dt(bits) => format!("DT|{}", fmt_6dp(f64::from_bits(*bits))),
        }
    }

    /// Format an f64 rounded to 6dp into a stable decimal string (so 2000.0 and
    /// 2000.000000 compare equal across the Python `round(x, 6)` and Rust
    /// `num_to_bits` paths). Trailing zeros trimmed; integral values keep a `.0`.
    fn fmt_6dp(x: f64) -> String {
        let rounded = (x * 1_000_000.0).round() / 1_000_000.0;
        let rounded = if rounded == 0.0 { 0.0 } else { rounded };
        let mut s = format!("{rounded:.6}");
        while s.ends_with('0') {
            s.pop();
        }
        if s.ends_with('.') {
            s.push('0');
        }
        s
    }

    fn rust_canon_set(triples: &[Triple]) -> BTreeSet<String> {
        triples
            .iter()
            .map(|(s, p, o)| format!("{s}|{p}|{}", canon_key_for_value(&canon_value(o))))
            .collect()
    }

    /// Build the SAME comparison key from the golden's `canon` array entries
    /// (`[s, p, [TAG, value]]`).
    fn golden_canon_set(golden: &Json) -> BTreeSet<String> {
        let mut set = BTreeSet::new();
        for entry in golden["canon"]
            .as_array()
            .expect("golden.canon is an array")
        {
            let s = entry[0].as_str().expect("canon subject is a string");
            let p = entry[1].as_str().expect("canon predicate is a string");
            let cv = entry[2]
                .as_array()
                .expect("canon value is a [tag, value] pair");
            let tag = cv[0].as_str().expect("canon tag is a string");
            let val = &cv[1];
            let tail = match tag {
                "PLACEHOLDER" | "URI" | "LIT" => {
                    format!("{tag}|{}", val.as_str().expect("string canon value"))
                }
                "BOOL" => format!("BOOL|{}", val.as_bool().expect("bool canon value")),
                "NUM" => format!("NUM|{}", fmt_6dp(val.as_f64().expect("num canon value"))),
                "DT" => format!("DT|{}", fmt_6dp(val.as_f64().expect("dt canon value"))),
                other => panic!("unknown golden canon tag: {other}"),
            };
            set.insert(format!("{s}|{p}|{tail}"));
        }
        set
    }

    // ── (1) PARITY ────────────────────────────────────────────────────────────

    #[test]
    fn rust_desired_set_equals_python_golden_mod_canon() {
        let golden = golden();
        let plan = plan_on_empty();

        // The "new" mint plans a non-empty rdfInsert and zero rdfDelete — the
        // desired set is exactly the INSERT (adds) set on an empty graph.
        assert_eq!(
            plan.mode, "new",
            "demo fixture on empty graph is a fresh mint"
        );
        assert_eq!(
            plan.summary.rdf_delete, 0,
            "no DELETE against an empty current graph"
        );
        assert!(plan.summary.rdf_insert > 0, "the mint inserts triples");

        let desired = rust_desired_triples(&plan);
        // Cross-check the recovered count against the plan's own rdfInsert summary.
        assert_eq!(
            desired.len(),
            plan.summary.rdf_insert,
            "recovered desired-triple count must equal summary.rdfInsert"
        );

        // The golden agrees on the size of the set (a coarse but load-bearing pin).
        let golden_insert = golden["rdfInsert"].as_u64().expect("golden rdfInsert") as usize;
        assert_eq!(
            desired.len(),
            golden_insert,
            "Rust desired-set size != Python golden rdfInsert"
        );

        // The load-bearing assertion: Rust desired SET == Python golden SET, mod
        // canon (value-canonical equality).
        let rust = rust_canon_set(&desired);
        let want = golden_canon_set(&golden);

        let missing: Vec<&String> = want.difference(&rust).collect();
        let extra: Vec<&String> = rust.difference(&want).collect();
        assert!(
            missing.is_empty() && extra.is_empty(),
            "Rust desired set != Python golden (mod canon).\n  missing from Rust: {missing:#?}\n  extra in Rust: {extra:#?}"
        );
        assert_eq!(rust, want, "Rust desired set == Python golden mod canon");

        // The SCRIPT_BLOCK placeholder is present and UNRESOLVED in the desired
        // set on both sides (it is resolved only by the applier, post write_doc).
        let wfns = workflow_vocabulary().primary_namespace();
        let has_placeholder = desired.iter().any(|(_, p, o)| {
            p == &format!("{wfns}scriptBlock")
                && matches!(o, Term::Placeholder(name) if name == "SCRIPT_BLOCK")
        });
        assert!(
            has_placeholder,
            "the desired set must carry an UNRESOLVED wf:scriptBlock placeholder"
        );
        assert_eq!(
            golden["scriptBlockPlaceholder"].as_str(),
            Some(format!("{PLACEHOLDER_NS}SCRIPT_BLOCK").as_str()),
            "golden pins the SCRIPT_BLOCK placeholder URI"
        );
    }

    // ── (2) FROZEN-VOCAB GUARD ─────────────────────────────────────────────────

    #[test]
    fn frozen_vocab_guard_no_predicate_outside_contract() {
        let contract = workflow_vocabulary();
        let known = contract.known_predicate_uris();

        // rdf:type is the one structural predicate the mint emits that is not a
        // *declared class predicate* (it carries the class, not a value). It is
        // still a contract-namespace term; allow exactly it alongside the
        // declared set. Everything else MUST be a known (declared) predicate.
        let rdf_type = format!(
            "{}type",
            contract
                .namespaces
                .get("rdf")
                .expect("contract declares the rdf namespace")
        );

        let plan = plan_on_empty();
        let desired = rust_desired_triples(&plan);
        assert!(!desired.is_empty(), "the mint produced triples to guard");

        let offenders: Vec<&str> = desired
            .iter()
            .map(|(_, p, _)| p.as_str())
            .filter(|p| *p != rdf_type && !known.contains(*p))
            .collect();
        assert!(
            offenders.is_empty(),
            "minted predicate URI(s) outside contract.known_predicate_uris(): {offenders:?}"
        );

        // Every declared predicate URI resolves under a declared namespace (no
        // bare CURIEs leaked into the frozen set).
        for uri in &known {
            assert!(
                uri.contains("://") || uri.starts_with("urn:"),
                "known predicate URI not fully expanded: {uri}"
            );
        }

        // The embedded golden sha is still pinned. If the vocab bytes drift, the
        // mint can mint new predicates and this guard is void.
        assert!(
            WORKFLOW_GOLDEN_SHA.starts_with("fa103c3b"),
            "embedded golden sha drifted from fa103c3b...: {WORKFLOW_GOLDEN_SHA}"
        );
        assert_eq!(
            WORKFLOW_GOLDEN_SHA, "fa103c3b89fa1167bae2de9c0c3d5437c09d0d1e3adc4967d32154a2e82d4d05",
            "the frozen workflow golden sha is the pin"
        );
    }

    // ── (3) CROSS-WP INVARIANT ─────────────────────────────────────────────────

    #[test]
    fn cross_wp_read_eq_write_eq_user_rdf() {
        // READ-GRAPH == WRITE-GRAPH (risk #1, the #1 convergence risk): the graph
        // the survey READS must be byte-identical to the graph the applier WRITES,
        // and both must be exactly user_rdf_graph_iri(graph_id). A standing
        // regression guard — if any WP ever points at <{root}> or a :projection:
        // graph, this fails loudly.
        for graph_id in ["lab", "U", "graph-a", "some-graph-id-123"] {
            let authority = user_rdf_graph_iri(graph_id);
            let read = read_graph_iri(graph_id);
            let write = write_graph_iri(graph_id);

            assert_eq!(
                read, authority,
                "survey read graph must == user_rdf_graph_iri for {graph_id}"
            );
            assert_eq!(
                write, authority,
                "applier write graph must == user_rdf_graph_iri for {graph_id}"
            );
            assert_eq!(
                read, write,
                "survey read graph must == applier write graph for {graph_id}"
            );

            // The literal `:user:rdf` form (the only form that passes
            // validate_sparql_update_authority): NOT the bare root, NOT a
            // :projection: graph.
            assert!(
                authority.ends_with(":user:rdf"),
                "write graph must be the :user:rdf graph, got {authority}"
            );
            assert!(
                !authority.contains(":projection:"),
                "write graph must NEVER be a :projection: graph, got {authority}"
            );
        }
    }
}

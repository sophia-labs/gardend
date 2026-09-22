//! Class dispatch — the one honest read of a class's declared
//! `MaterializationSignature` into the [`DispatchRoute`] that names which
//! apply-family fork actually runs it.
//!
//! T5.22 (the ratified synthesis) named the debt this module retires: every
//! golden pack declares a per-class [`crate::emporium::contract::MaterializationSignature`]
//! (`source_kind` / `identity_kind` / `store_mode` / `store_target` /
//! `enforcement` / `reconciliation_strategy` / `dispatch_mode`), but almost
//! nothing in the ingest spine actually CONSULTS it — the memory family
//! (`plan_memory_compute` / `Plan::routes_to_memory_sink`) and the
//! workflow/campaign fallthrough dispatch on a `plan.mode` string that is set
//! by a hardcoded literal and checked against another hardcoded literal, with
//! zero structural connection to the pack's own declaration. Only the
//! GENERIC simple-projection fork's per-class materialize/virtual partition
//! (`spine::materialized_class_partitions`, and its sibling per-record check
//! in `planner::plan_generic_compute`) ever actually read `store_mode`.
//!
//! [`resolve`] is the pure `(contract, class_name) -> signature + route`
//! function every one of those call sites now goes through. It is
//! deliberately dumb: the route is fully determined by `store_mode` +
//! `store_target` (the class's own declared storage shape), never by the
//! class's NAME, the request's vocab string, or which top-level function
//! happens to be calling it.
//!
//! `reconciliation_strategy` is the OTHER signature field the synthesis named
//! as inert (parsed in `contract.rs`, read nowhere else). It stays inert
//! here on purpose: every route implemented today — the direct-on-store
//! memory materializer's contested-lineage sweep
//! ([`crate::emporium::sweep`]), the generic `reconcile_classes_validated`
//! primitive, the workflow/campaign CRDT applier — is bespoke CODE, not a
//! declaratively-selected strategy, so there is no resolver yet for
//! [`resolve`] to hand off to. [`resolve`] is shared by every call site in
//! this module's header (`plan_memory_compute`, `plan_generic_compute`,
//! `materialized_class_partitions`), which iterate every class a contract
//! declares, not just the classes a given request touches — gating on this
//! field HERE would make a single class's declared strategy a hard failure
//! for every OTHER class's dispatch in the same contract. Wiring
//! `reconciliation_strategy` into an actual halt/route decision is future
//! work for whichever call site needs it, scoped to that call site's own
//! policy — not a behavior this shared resolver should assert.

use crate::emporium::contract::{MaterializationSignature, StoreMode, VocabularyContract};

/// Which apply-family fork a class's declared signature resolves to — the
/// four write-sink shapes the ingest spine implements today.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DispatchRoute {
    /// `store_target == "projection:memory"` — the direct-on-store memory
    /// materializer (`apply_memory_plan`, per-observer graph, the EA-6 SHACL
    /// gate, the sweep.rs contested-lineage machinery).
    MemorySink,
    /// `store_mode == materialize` and `store_target` is a `projection:*`
    /// sink OTHER than memory — the generic `reconcile_classes_validated`
    /// primitive (`apply_simple_projection_plan`).
    SimpleProjection,
    /// `store_mode == materialize` and `store_target` is not a `projection:*`
    /// sink (`user:rdf`, or any other code-owned target) — the workflow/
    /// campaign CRDT applier (`apply_plan`), reached by elimination in
    /// `apply_and_assert` (whatever is neither memory nor simple-projection).
    WorkflowCampaign,
    /// `store_mode == virtual` — read-resolved (`derived_from_query`), never
    /// materialized; excluded from every write partition.
    VirtualSkip,
}

impl DispatchRoute {
    /// The `Plan.mode` / `ApplyReport.mode` string label this route mints
    /// under. The single named source for these literals: before this
    /// module, `plan_memory_compute` hardcoded `"memory"` and
    /// `Plan::routes_to_memory_sink` independently hardcoded the same
    /// literal to compare against it — two coincidentally-matching string
    /// constants, not one declaration. `WorkflowCampaign` / `VirtualSkip`
    /// are never minted as a `Plan.mode` (the workflow/campaign family uses
    /// its own per-render mode strings, and a virtual class is never
    /// planned at all) — their labels exist for the route table / logging.
    pub(crate) const fn mode_label(self) -> &'static str {
        match self {
            Self::MemorySink => "memory",
            Self::SimpleProjection => "simple-projection",
            Self::WorkflowCampaign => "workflow-campaign",
            Self::VirtualSkip => "virtual-skip",
        }
    }
}

/// A class's declared [`MaterializationSignature`] plus the [`DispatchRoute`]
/// it resolves to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ClassDispatch<'a> {
    pub(crate) signature: MaterializationSignature<'a>,
    pub(crate) route: DispatchRoute,
}

/// Pure: `(contract, class_name) -> the class's declared signature + the
/// route it resolves to`. Errors exactly where
/// [`VocabularyContract::materialization_signature`] would (a missing/
/// invalid field) — no additional rejection is layered on here (see the
/// module doc on `reconciliation_strategy`).
///
/// The route is derived ONLY from `store_mode` + `store_target` — no vocab
/// name, no request shape, no `plan.mode`. This is deliberate: it is the
/// thing every call site below used to decide via a hardcoded string, now
/// reading the pack's own declaration instead.
pub(crate) fn resolve<'a>(
    contract: &'a VocabularyContract,
    class_name: &str,
) -> Result<ClassDispatch<'a>, String> {
    let signature = contract.materialization_signature(class_name)?;

    let route = if signature.store_mode == StoreMode::Virtual {
        DispatchRoute::VirtualSkip
    } else if signature.store_target == "projection:memory" {
        DispatchRoute::MemorySink
    } else if signature.store_target.starts_with("projection:") {
        DispatchRoute::SimpleProjection
    } else {
        DispatchRoute::WorkflowCampaign
    };

    // `reconciliation_strategy` is deliberately NOT read here (see module
    // doc): this resolver is shared by call sites that iterate every class
    // in a contract, so gating on a field unrelated to `store_mode` /
    // `store_target` would make one class's declaration a hard failure for
    // every other class's dispatch. The route is store_mode/store_target
    // ONLY, exactly as it was before this module existed.

    Ok(ClassDispatch { signature, route })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::emporium::contract::get_vocabulary;
    use crate::emporium::vocabs::VOCAB_REGISTRY;
    use serde_json::json;

    /// The exhaustive route table over every class in every SERVED pack
    /// (`VOCAB_REGISTRY` — the same set `contract::tests::
    /// every_served_class_has_a_valid_materialization_signature` already
    /// proves has a full signature). This is the SNAPSHOT: edit a pack's
    /// `store_mode` / `store_target`, add a class, or register a new pack,
    /// and this test's diff shows exactly which class's route moved.
    #[test]
    fn every_served_class_resolves_to_the_declared_route() {
        let mut table: Vec<(&str, &str, &str)> = Vec::new();
        for (pack, _json, _sha) in VOCAB_REGISTRY.iter() {
            let contract = get_vocabulary(pack).unwrap_or_else(|| panic!("{pack} resolves"));
            for class_name in contract.classes.keys() {
                let dispatch = resolve(contract, class_name)
                    .unwrap_or_else(|error| panic!("{pack}.{class_name}: {error}"));
                table.push((*pack, class_name.as_str(), dispatch.route.mode_label()));
            }
        }
        table.sort();

        let expected: &[(&str, &str, &str)] = &[
            ("emporium-bookmark", "Bookmark", "simple-projection"),
            ("emporium-chamber", "DomainOntology", "simple-projection"),
            ("emporium-observatory", "CapacityEstimate", "virtual-skip"),
            ("emporium-observatory", "CaptureEvent", "virtual-skip"),
            ("emporium-observatory", "FleetObservation", "virtual-skip"),
            ("emporium-observatory", "MachineRun", "virtual-skip"),
            ("emporium-observatory", "MetricDefinition", "virtual-skip"),
            ("emporium-observatory", "MetricEvaluation", "virtual-skip"),
            ("emporium-observatory", "MetricObservation", "virtual-skip"),
            ("emporium-observatory", "ProjectionRun", "virtual-skip"),
            ("emporium-observatory", "SequenceGap", "virtual-skip"),
            ("emporium-observatory", "SourceSnapshot", "virtual-skip"),
            ("emporium-observatory", "SpawnAttempt", "virtual-skip"),
            ("garden-pdf-source", "PdfOriginal", "simple-projection"),
            ("garden-pdf-source", "PdfPageSelector", "simple-projection"),
            ("garden-pdf-source", "PdfRegion", "simple-projection"),
            ("garden-pdf-source", "TextSourceAnchor", "simple-projection"),
            ("kg-ultra-intuition", "AnswerCandidate", "simple-projection"),
            ("kg-ultra-intuition", "Intuition", "simple-projection"),
            (
                "kg-ultra-intuition",
                "IntuitionCandidate",
                "simple-projection",
            ),
            ("koch-morse", "CopyAttempt", "simple-projection"),
            ("koch-morse", "KeyingAttempt", "simple-projection"),
            ("koch-morse", "KochCourse", "simple-projection"),
            ("koch-morse", "Learner", "simple-projection"),
            ("koch-morse", "Lesson", "simple-projection"),
            ("koch-morse", "MorseCharacter", "simple-projection"),
            ("koch-morse", "PracticeSession", "simple-projection"),
            ("lex-scotus-core", "Case", "simple-projection"),
            ("lex-scotus-core", "DoctrinalTest", "simple-projection"),
            ("lex-scotus-core", "DoctrineHead", "simple-projection"),
            ("lex-scotus-core", "Holding", "simple-projection"),
            ("lex-scotus-core", "Justice", "simple-projection"),
            ("lex-scotus-core", "LegalQuestion", "simple-projection"),
            ("lex-scotus-core", "Opinion", "simple-projection"),
            ("lex-scotus-core", "ReasoningStep", "simple-projection"),
            (
                "lme-labeled-memory",
                "AdjudicationEvent",
                "simple-projection",
            ),
            (
                "lme-labeled-memory",
                "BackprojectionCheck",
                "simple-projection",
            ),
            ("lme-labeled-memory", "CasePacket", "simple-projection"),
            ("lme-labeled-memory", "ConceptCue", "simple-projection"),
            ("lme-labeled-memory", "Derivation", "simple-projection"),
            ("lme-labeled-memory", "EvidenceSpan", "simple-projection"),
            ("lme-labeled-memory", "MemoryFrame", "simple-projection"),
            ("lme-labeled-memory", "Observation", "simple-projection"),
            ("lme-labeled-memory", "QuestionCase", "simple-projection"),
            ("ludus-core", "AttemptSubmitted", "simple-projection"),
            ("ludus-core", "Concept", "simple-projection"),
            ("ludus-core", "Encounter", "simple-projection"),
            ("ludus-core", "FeedbackRecorded", "simple-projection"),
            ("ludus-core", "SourceEdition", "simple-projection"),
            ("ludus-core", "SourceOccurrence", "simple-projection"),
            ("shrubbery-site", "ComponentBinding", "simple-projection"),
            ("shrubbery-site", "ContentSource", "simple-projection"),
            ("shrubbery-site", "PackProvenance", "simple-projection"),
            ("shrubbery-site", "PublicationRoute", "simple-projection"),
            ("shrubbery-site", "Route", "simple-projection"),
            ("shrubbery-site", "SiteDefinition", "simple-projection"),
            ("shrubbery-site", "Surface", "simple-projection"),
            ("shrubbery-site", "Theme", "simple-projection"),
            ("sophia-agent-core", "Agent", "workflow-campaign"),
            ("sophia-agent-core", "Capability", "workflow-campaign"),
            ("sophia-agent-core", "Driver", "workflow-campaign"),
            ("sophia-agent-core", "Faculty", "workflow-campaign"),
            ("sophia-agent-core", "FacultyPooling", "workflow-campaign"),
            ("sophia-agent-core", "PoolingMode", "workflow-campaign"),
            ("sophia-agent-core", "ReadOperation", "workflow-campaign"),
            ("sophia-agent-core", "Run", "workflow-campaign"),
            ("sophia-agent-core", "Session", "workflow-campaign"),
            ("sophia-agent-core", "Tool", "workflow-campaign"),
            ("sophia-agent-core", "Turn", "workflow-campaign"),
            ("sophia-agent-core", "Voice", "workflow-campaign"),
            ("sophia-agent-core", "Voicing", "workflow-campaign"),
            ("sophia-agent-core", "Witness", "workflow-campaign"),
            ("sophia-api", "Operation", "simple-projection"),
            ("sophia-api", "Parameter", "simple-projection"),
            ("sophia-api", "Response", "simple-projection"),
            ("sophia-api", "Server", "simple-projection"),
            ("sophia-api", "WorkflowBinding", "simple-projection"),
            (
                "sophia-domain-dashboard",
                "DashboardSurface",
                "simple-projection",
            ),
            (
                "sophia-domain-manifest",
                "CapabilityClaim",
                "simple-projection",
            ),
            (
                "sophia-domain-manifest",
                "DomainManifest",
                "simple-projection",
            ),
            ("sophia-domain-manifest", "Journey", "simple-projection"),
            ("sophia-domain-manifest", "JourneyStep", "simple-projection"),
            ("sophia-domain-manifest", "Mode", "simple-projection"),
            ("sophia-domain-manifest", "Role", "simple-projection"),
            ("sophia-domain-manifest", "Tier", "simple-projection"),
            ("sophia-domain-verdict", "EvidenceRef", "simple-projection"),
            ("sophia-domain-verdict", "Verdict", "simple-projection"),
            ("sophia-machine-core", "Binding", "simple-projection"),
            ("sophia-machine-core", "Machine", "simple-projection"),
            (
                "sophia-machine-core",
                "MachineDefinition",
                "simple-projection",
            ),
            ("sophia-machine-core", "MachineRun", "simple-projection"),
            ("sophia-machine-core", "Port", "simple-projection"),
            ("sophia-memory-core", "Claim", "memory"),
            ("sophia-memory-core", "EvidenceLink", "memory"),
            ("sophia-memory-core", "MemoryRecord", "memory"),
            ("sophia-memory-core", "Policy", "memory"),
            ("sophia-memory-core", "SourceReference", "memory"),
            ("wf-agent-binding", "AgentRunBinding", "workflow-campaign"),
            ("wf-agent-binding", "RunBinding", "workflow-campaign"),
            (
                "wf-agent-binding",
                "SessionRealization",
                "workflow-campaign",
            ),
            ("wf-agent-binding", "TurnRealization", "workflow-campaign"),
            ("wf-agent-session-projection", "Agent", "simple-projection"),
            ("wf-agent-session-projection", "Run", "simple-projection"),
            (
                "wf-agent-session-projection",
                "Session",
                "simple-projection",
            ),
            ("wf-agent-session-projection", "Tool", "simple-projection"),
            ("wf-agent-session-projection", "Turn", "simple-projection"),
            ("wf-agent-session-projection", "Voice", "simple-projection"),
            (
                "wf-agent-session-projection",
                "WorkflowAgentRunAnchor",
                "simple-projection",
            ),
            (
                "wf-agent-session-projection",
                "WorkflowRunAnchor",
                "simple-projection",
            ),
            (
                "wf-agent-world-runtime",
                "PromptBinding",
                "simple-projection",
            ),
            (
                "wf-agent-world-runtime",
                "SessionApproval",
                "simple-projection",
            ),
            (
                "wf-agent-world-runtime",
                "SessionComment",
                "simple-projection",
            ),
            (
                "wf-agent-world-runtime",
                "SessionMessage",
                "simple-projection",
            ),
            (
                "wf-agent-world-runtime",
                "SessionState",
                "simple-projection",
            ),
            ("workflow", "ActionArgument", "workflow-campaign"),
            ("workflow", "AgentNode", "workflow-campaign"),
            ("workflow", "AgentRun", "workflow-campaign"),
            ("workflow", "Archetype", "workflow-campaign"),
            ("workflow", "AuthoringSession", "workflow-campaign"),
            ("workflow", "AuthorizationFlag", "workflow-campaign"),
            ("workflow", "CompletenessGap", "virtual-skip"),
            ("workflow", "CompositionEvent", "workflow-campaign"),
            ("workflow", "Contract", "workflow-campaign"),
            ("workflow", "Draft", "virtual-skip"),
            ("workflow", "DraftWarning", "virtual-skip"),
            ("workflow", "EvidenceArtifact", "workflow-campaign"),
            ("workflow", "LiveRecommendedAction", "virtual-skip"),
            ("workflow", "NavigationRoute", "workflow-campaign"),
            ("workflow", "Operation", "simple-projection"),
            ("workflow", "PageTurnDecision", "workflow-campaign"),
            ("workflow", "PageView", "workflow-campaign"),
            ("workflow", "Parameter", "simple-projection"),
            ("workflow", "Phase", "workflow-campaign"),
            ("workflow", "Protocol", "workflow-campaign"),
            ("workflow", "RawSparqlQuery", "workflow-campaign"),
            ("workflow", "RecommendedAction", "workflow-campaign"),
            ("workflow", "Response", "simple-projection"),
            ("workflow", "RouteAction", "workflow-campaign"),
            ("workflow", "Run", "workflow-campaign"),
            ("workflow", "RunStatistics", "virtual-skip"),
            ("workflow", "Server", "simple-projection"),
            ("workflow", "Variant", "workflow-campaign"),
            ("workflow", "Workflow", "workflow-campaign"),
            ("workflow", "WorkflowBinding", "simple-projection"),
            ("workflow-ui", "AuthorizationFlag", "workflow-campaign"),
            ("workflow-ui", "EvidenceArtifact", "workflow-campaign"),
            ("workflow-ui", "NavigationRoute", "workflow-campaign"),
            ("workflow-ui", "PageTurnDecision", "workflow-campaign"),
            (
                "workflow-ui",
                "WorkflowAdventurePacket",
                "workflow-campaign",
            ),
        ];
        assert_eq!(
            table.len(),
            expected.len(),
            "class count drifted (a pack registered/deregistered a class) — got: {table:#?}"
        );
        assert_eq!(
            table, expected,
            "the declared route table moved — a pack edit changed a class's dispatch route"
        );
    }

    /// A synthetic materialize-shaped class, only `store_target` and
    /// `reconciliation_strategy` varying — the minimal pair for the
    /// load-bearing proof below.
    fn synthetic_materialize_contract(
        store_target: &str,
        reconciliation_strategy: &str,
    ) -> VocabularyContract {
        serde_json::from_value(json!({
            "name": "demo-dispatch",
            "version": "1.0.0",
            "title": "Demo Dispatch",
            "description": "demo",
            "namespaces": {
                "demo": "http://example.test/demo#",
                "rdf": "http://www.w3.org/1999/02/22-rdf-syntax-ns#",
                "xsd": "http://www.w3.org/2001/XMLSchema#"
            },
            "primary_prefix": "demo",
            "classes": {
                "Widget": {
                    "rdf_types": ["demo:Widget"],
                    "subject_rule": "{graph_subject}:projection:demo:{localId}",
                    "source_kind": "current-state",
                    "identity_kind": "urn-template",
                    "store_mode": "materialize",
                    "store_target": store_target,
                    "enforcement": "halt",
                    "reconciliation_strategy": reconciliation_strategy,
                    "dispatch_mode": "current-state-materialize",
                    "predicates": {
                        "demo:label": {"datatype": "string", "required": false}
                    }
                }
            }
        }))
        .expect("synthetic materialize contract parses")
    }

    /// A synthetic virtual-shaped class (satisfies the `materialization_signature`
    /// virtual-mode cross-field invariants: derived + resolve-by-query +
    /// derived_from_query).
    fn synthetic_virtual_contract() -> VocabularyContract {
        serde_json::from_value(json!({
            "name": "demo-dispatch-virtual",
            "version": "1.0.0",
            "title": "Demo Dispatch Virtual",
            "description": "demo",
            "namespaces": {
                "demo": "http://example.test/demo#",
                "rdf": "http://www.w3.org/1999/02/22-rdf-syntax-ns#",
                "xsd": "http://www.w3.org/2001/XMLSchema#"
            },
            "primary_prefix": "demo",
            "classes": {
                "Widget": {
                    "rdf_types": ["demo:Widget"],
                    "subject_rule": "virtual:resolve-by-query:demo.widget",
                    "source_kind": "derived",
                    "identity_kind": "resolve-by-query",
                    "store_mode": "virtual",
                    "store_target": "none",
                    "enforcement": "warning",
                    "reconciliation_strategy": "codeBacked",
                    "dispatch_mode": "derived-virtual",
                    "derived_from_query": "demo.widget(test slice)",
                    "predicates": {
                        "demo:label": {"datatype": "string", "required": false}
                    }
                }
            }
        }))
        .expect("synthetic virtual contract parses")
    }

    /// LOAD-BEARING PROOF (distinguishes honest wiring from a decorative
    /// read): the SAME class, only its declared `store_target`/`store_mode`
    /// differing, resolves to a DIFFERENT route. Nothing here names "Widget"
    /// specially — the route moves purely because the declaration moved.
    #[test]
    fn flipping_store_target_moves_the_class_across_dispatch_routes() {
        let memory = synthetic_materialize_contract("projection:memory", "codeBacked");
        assert_eq!(
            resolve(&memory, "Widget").unwrap().route,
            DispatchRoute::MemorySink
        );

        let projection = synthetic_materialize_contract("projection:bookmark", "codeBacked");
        assert_eq!(
            resolve(&projection, "Widget").unwrap().route,
            DispatchRoute::SimpleProjection
        );

        let user_rdf = synthetic_materialize_contract("user:rdf", "codeBacked");
        assert_eq!(
            resolve(&user_rdf, "Widget").unwrap().route,
            DispatchRoute::WorkflowCampaign
        );

        let virtual_contract = synthetic_virtual_contract();
        assert_eq!(
            resolve(&virtual_contract, "Widget").unwrap().route,
            DispatchRoute::VirtualSkip
        );
    }

    /// `reconciliation_strategy` stays inert here (module doc): `resolve` is
    /// shared by call sites that iterate every class in a contract, so a
    /// value this engine has no resolver for must NOT become a hard error
    /// for classes that have nothing to do with it. A valid but
    /// non-`codeBacked` declaration keeps resolving, and the route it
    /// resolves to is unaffected — driven only by `store_mode`/`store_target`,
    /// same as `codeBacked`.
    #[test]
    fn reconciliation_strategy_is_parsed_but_does_not_gate_or_change_the_route() {
        for value in [
            "codeBacked",
            "contested",
            "producerDirected",
            "causalLww",
            "evidenceWeighted",
        ] {
            let contract = synthetic_materialize_contract("projection:memory", value);
            let dispatch = resolve(&contract, "Widget")
                .unwrap_or_else(|error| panic!("{value} should still resolve: {error}"));
            assert_eq!(
                dispatch.route,
                DispatchRoute::MemorySink,
                "{value}: reconciliation_strategy must not move the route"
            );
        }
    }

    /// Every registered class must resolve — a class with no signature at all
    /// (a legacy pre-signature pack, never registered in `VOCAB_REGISTRY`)
    /// is out of scope for THIS table (see the module doc); a REGISTERED
    /// class always has one (enforced by `materialization_signature` itself).
    #[test]
    fn every_registered_class_is_accounted_for_in_the_table() {
        let total: usize = VOCAB_REGISTRY
            .iter()
            .map(|(pack, _json, _sha)| get_vocabulary(pack).unwrap().classes.len())
            .sum();
        assert_eq!(
            total, 139,
            "VOCAB_REGISTRY class count drifted from the snapshot above"
        );
    }

    /// N1 landing-brief step 2: `DoctrineHead` declares `reconciliation_strategy:
    /// "contested"` on a `SimpleProjection` route (`store_target:
    /// "projection:lex"`). Prove — by test, not by reading the module doc alone
    /// — that the spine's shared `resolve` ACCEPTS this: `reconciliation_strategy`
    /// is parsed but never gates or changes the route (see
    /// `reconciliation_strategy_is_parsed_but_does_not_gate_or_change_the_route`
    /// above for the synthetic-contract proof; this is the SAME invariant over
    /// the REAL served `lex-scotus-core` pack, so a future edit to the golden's
    /// `DoctrineHead.reconciliation_strategy` cannot silently start halting).
    #[test]
    fn lex_scotus_core_doctrine_head_contested_resolves_to_simple_projection() {
        let contract = get_vocabulary("lex-scotus-core").expect("lex-scotus-core registered (N1)");
        let dispatch = resolve(contract, "DoctrineHead")
            .expect("contested reconciliation_strategy must not halt dispatch");
        assert_eq!(dispatch.route, DispatchRoute::SimpleProjection);
        assert_eq!(
            dispatch.signature.reconciliation_strategy,
            crate::emporium::contract::ReconciliationStrategy::Contested
        );
    }
}

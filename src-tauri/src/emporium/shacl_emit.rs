//! Vocab → SHACL emitter — the VALIDATION FACE of a Meaningful-Objects vocab
//! contract (EA-1).
//!
//! A vocab contract is the single source of truth for `{survey-span, Lean
//! propositions, SHACL shapes}`. This module is the third leg: it DERIVES SHACL
//! NodeShapes (in Turtle) from a parsed [`VocabularyContract`] — never
//! hand-written, always a pure function of the contract. The shapes are fed to
//! the rudof validator just before the store write (see [`super::shacl_validator`]).
//!
//! The mapping is the obvious structural one, identical in spirit to the
//! Python `vocab_to_lean.py` POC (read JSON → extract invariants → template the
//! target form), but emitting SHACL instead of Lean:
//!
//! | contract                          | SHACL                                   |
//! |-----------------------------------|-----------------------------------------|
//! | a `ClassSpec` with `rdf_types`    | one `sh:NodeShape` per rdf_type target  |
//! | `ClassSpec.rdf_types[i]`          | `sh:targetClass <expanded>`             |
//! | every declared predicate (closed) | `sh:closed true` + `sh:ignoredProperties (rdf:type)` |
//! | `PredicateSpec` (literal datatype)| `sh:property [ sh:path …; sh:datatype xsd:… ]` |
//! | `PredicateSpec` (datatype = uri)  | `sh:property [ sh:path …; sh:nodeKind sh:IRI ]` |
//! | `required: true`                  | `sh:minCount 1`                         |
//! | `multi: false`                    | `sh:maxCount 1` (omitted when `multi`)  |
//!
//! Classes with NO `rdf_types` (e.g. the `wf:` `Protocol`, registry-only and
//! never minted) emit NO shape: there is no instance in the store to target, so
//! a closed shape with no target would be inert at best and a foot-gun at worst.
//!
//! The output is deterministic (classes + predicates iterate in `BTreeMap`
//! order) and parses as valid Turtle (the validator round-trips it through
//! rudof's `ShaclDataManager::load`, which is the load-bearing parse check).

use crate::emporium::contract::{Datatype, VocabularyContract};

/// The XSD datatype IRI for a literal [`Datatype`]. `uri` is intentionally
/// absent — object properties are constrained by `sh:nodeKind sh:IRI`, not
/// `sh:datatype`, so callers must branch on `Datatype::uri` before reaching here.
fn datatype_to_xsd(dt: Datatype) -> &'static str {
    match dt {
        Datatype::string => "http://www.w3.org/2001/XMLSchema#string",
        Datatype::integer => "http://www.w3.org/2001/XMLSchema#integer",
        Datatype::long => "http://www.w3.org/2001/XMLSchema#long",
        // xsd:float and xsd:double are DISTINCT SHACL datatypes (exact-IRI match).
        // The salience materializer emits ^^xsd:float; mapping it here lets the
        // derived shape match the real projection.
        Datatype::float => "http://www.w3.org/2001/XMLSchema#float",
        Datatype::double => "http://www.w3.org/2001/XMLSchema#double",
        Datatype::boolean => "http://www.w3.org/2001/XMLSchema#boolean",
        Datatype::dateTime => "http://www.w3.org/2001/XMLSchema#dateTime",
        // Object property — handled by the caller via sh:nodeKind, never here.
        Datatype::uri => "http://www.w3.org/2001/XMLSchema#anyURI",
    }
}

const RDF_TYPE_URI: &str = "http://www.w3.org/1999/02/22-rdf-syntax-ns#type";

/// The SHACL shapes-graph IRI a contract's shapes are minted under. Stable per
/// contract name so the same contract always derives the same shapes subject
/// space (the shapes are content-equivalent across runs).
pub(crate) fn shapes_namespace(contract: &VocabularyContract) -> String {
    format!("urn:sophia:shacl:{}#", contract.name)
}

/// The NodeShape subject IRI for a class. Distinct per (contract, class) so two
/// contracts' shapes never collide if loaded into one graph.
fn node_shape_iri(contract: &VocabularyContract, class_name: &str) -> String {
    format!("{}{}Shape", shapes_namespace(contract), class_name)
}

/// Emit the SHACL shapes graph (Turtle) for a whole vocab contract. One
/// `sh:NodeShape` per class that has at least one `rdf_type` target; each shape
/// is `sh:closed` (only declared predicates + `rdf:type` allowed) and carries
/// one `sh:property` per declared predicate with the structural constraints
/// (datatype/nodeKind, minCount, maxCount) read straight off the contract.
///
/// Pure function of the contract — no I/O, no store, deterministic.
pub(crate) fn vocab_to_shacl(contract: &VocabularyContract) -> String {
    let mut out = String::new();
    out.push_str("@prefix sh: <http://www.w3.org/ns/shacl#> .\n");
    out.push_str("@prefix rdf: <http://www.w3.org/1999/02/22-rdf-syntax-ns#> .\n");
    out.push_str("@prefix xsd: <http://www.w3.org/2001/XMLSchema#> .\n\n");

    for (class_name, class) in &contract.classes {
        // Classes with no rdf:type cannot be targeted (registry-only, never
        // minted) — emit no shape (see module docs).
        if class.rdf_types.is_empty() {
            continue;
        }

        // Target ONLY the class-discriminating rdf:types — those in the
        // contract's PRIMARY namespace (e.g. mem:SourceReference). Foreign
        // upper-ontology types (e.g. prov:Entity) are deliberately NOT targeted:
        // they are SHARED across classes (both SourceReference and EvidenceLink
        // are a prov:Entity), so targeting them would make EvidenceLink's shape
        // also fire on every SourceReference — cross-class target bleed that
        // wrongly demands the sister class's required predicates. The primary
        // type is the discriminator; the foreign types are still ASSERTED on the
        // instance (and permitted by the closed shape via rdf:type), just not
        // used as a shape target. A class with no primary-namespace rdf:type is
        // not targetable here and emits no shape.
        let primary_ns = contract.primary_namespace();
        let target_types: Vec<String> = class
            .rdf_types
            .iter()
            .filter_map(|t| contract.expand(t).ok())
            .filter(|uri| uri.starts_with(primary_ns))
            .collect();
        if target_types.is_empty() {
            continue;
        }

        let shape_iri = node_shape_iri(contract, class_name);
        out.push_str(&format!("<{shape_iri}> a sh:NodeShape ;\n"));
        for type_uri in &target_types {
            out.push_str(&format!("    sh:targetClass <{type_uri}> ;\n"));
        }

        // Closed shape: only the declared predicates (plus rdf:type, which is
        // structural and present on every instance) are permitted. A rogue
        // predicate outside the contract is a violation — the SHACL image of the
        // frozen-vocab guard.
        out.push_str("    sh:closed true ;\n");
        out.push_str(&format!(
            "    sh:ignoredProperties ( <{RDF_TYPE_URI}> ) ;\n"
        ));

        for (curie, pred) in &class.predicates {
            let path = match contract.expand(curie) {
                Ok(uri) => uri,
                // A predicate CURIE that does not expand is a contract bug; skip
                // it rather than emit a syntactically-broken shape.
                Err(_) => continue,
            };
            out.push_str("    sh:property [\n");
            out.push_str(&format!("        sh:path <{path}> ;\n"));
            match pred.datatype {
                Datatype::uri => {
                    // Object property — constrain to IRI nodes, not a datatype.
                    out.push_str("        sh:nodeKind sh:IRI ;\n");
                }
                literal => {
                    out.push_str(&format!(
                        "        sh:datatype <{}> ;\n",
                        datatype_to_xsd(literal)
                    ));
                }
            }
            if pred.required {
                out.push_str("        sh:minCount 1 ;\n");
            }
            if !pred.multi {
                out.push_str("        sh:maxCount 1 ;\n");
            }
            out.push_str("    ] ;\n");
        }

        // Terminate the shape (replace the trailing " ;\n" with " .\n").
        if out.ends_with(" ;\n") {
            out.truncate(out.len() - " ;\n".len());
            out.push_str(" .\n\n");
        } else {
            out.push_str(".\n\n");
        }
    }

    // Append the contract's RAW §3 shapes VERBATIM (the membrane-scoped, cross-
    // record, cardinality `sh:sparql`/`sh:select` invariants that CANNOT be derived
    // from the class/predicate structure — the spec §3 agent-ontology shapes:
    // I1-membrane, I1b, I2, I2b, I3-cardinality, I4b, IdentityUnification, the
    // advisories). Without this, those shapes are inert text on the golden and never
    // reach `validate_against_shapes` — so neither the rudof structural pass NOR the
    // oxigraph `sh:select` evaluator ([`super::shacl_sparql`]) ever fires them. The
    // merged graph drives BOTH faces into one [`super::shacl_validator::ViolationRecord`]
    // list. A contract with only derivable structure carries `None` here (no-op append).
    //
    // The raw shapes declare their own `@prefix` lines, so they concatenate cleanly
    // after the derived block (Turtle prefix declarations are statement-scoped and may
    // re-declare). The raw block is appended whole; the `sh:sparql` constraints are
    // recovered by the meta-model query, not by the structural emitter's templating.
    if let Some(raw) = &contract.raw_shacl_shapes {
        if !out.ends_with('\n') {
            out.push('\n');
        }
        out.push('\n');
        out.push_str(raw);
        if !out.ends_with('\n') {
            out.push('\n');
        }
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::emporium::contract::memory_core_vocabulary;

    #[test]
    fn emits_a_nodeshape_per_targetable_class() {
        let shacl = vocab_to_shacl(memory_core_vocabulary());
        // The five memory-core classes all carry rdf_types → all get a shape.
        for cls in [
            "Claim",
            "EvidenceLink",
            "MemoryRecord",
            "Policy",
            "SourceReference",
        ] {
            assert!(
                shacl.contains(&format!("urn:sophia:shacl:sophia-memory-core#{cls}Shape")),
                "missing NodeShape for {cls}"
            );
        }
        assert_eq!(
            shacl.matches("a sh:NodeShape").count(),
            5,
            "exactly five NodeShapes (one per memory-core class)"
        );
    }

    #[test]
    fn targets_primary_namespace_types_only_not_shared_upper_ontology() {
        let shacl = vocab_to_shacl(memory_core_vocabulary());
        // MemoryRecord targets mem:MemoryRecord (primary namespace).
        assert!(shacl.contains("sh:targetClass <http://mnemosyne.dev/memory#MemoryRecord>"));
        // EvidenceLink targets its PRIMARY type mem:EvidenceLink …
        assert!(shacl.contains("sh:targetClass <http://mnemosyne.dev/memory#EvidenceLink>"));
        // … but NOT the shared upper-ontology prov:Entity (would bleed across
        // classes: SourceReference is ALSO a prov:Entity).
        assert!(
            !shacl.contains("sh:targetClass <http://www.w3.org/ns/prov#Entity>"),
            "shared foreign types must NOT be shape targets (cross-class bleed)"
        );
    }

    #[test]
    fn required_predicate_gets_min_count_one() {
        let shacl = vocab_to_shacl(memory_core_vocabulary());
        // mem:content is required on MemoryRecord → minCount 1; it is a string.
        // Find the property block for mem:content and assert its constraints.
        let needle = "sh:path <http://mnemosyne.dev/memory#content> ;";
        let pos = shacl.find(needle).expect("mem:content property present");
        let block = &shacl[pos..pos + 200];
        assert!(
            block.contains("sh:datatype <http://www.w3.org/2001/XMLSchema#string>"),
            "mem:content is xsd:string"
        );
        assert!(block.contains("sh:minCount 1"), "mem:content is required");
        assert!(
            block.contains("sh:maxCount 1"),
            "mem:content is single-valued"
        );
    }

    #[test]
    fn object_property_uses_nodekind_iri_not_datatype() {
        let shacl = vocab_to_shacl(memory_core_vocabulary());
        // mem:derivedFrom is a required, MULTI uri (object) property.
        let needle = "sh:path <http://mnemosyne.dev/memory#derivedFrom> ;";
        let pos = shacl
            .find(needle)
            .expect("mem:derivedFrom property present");
        let block = &shacl[pos..pos + 160];
        assert!(block.contains("sh:nodeKind sh:IRI"), "uri → nodeKind IRI");
        assert!(block.contains("sh:minCount 1"), "derivedFrom required (I1)");
        // multi=true → NO maxCount.
        assert!(
            !block.contains("sh:maxCount"),
            "multi-valued predicate must NOT carry sh:maxCount"
        );
    }

    #[test]
    fn shape_is_closed_with_rdf_type_ignored() {
        let shacl = vocab_to_shacl(memory_core_vocabulary());
        assert!(shacl.contains("sh:closed true"), "shapes are closed");
        assert!(
            shacl.contains(
                "sh:ignoredProperties ( <http://www.w3.org/1999/02/22-rdf-syntax-ns#type> )"
            ),
            "rdf:type is ignored so the closed shape does not reject it"
        );
    }

    // ── N1 landing-brief step 4: lex-scotus-core SHACL emission assertions ──

    /// `DoctrineHead`'s lineage-machinery predicates are all `required: true` in
    /// the golden — the derived shape must carry `sh:minCount 1` on each
    /// (`lex:lineage`/`lex:isCurrent`/`lex:createdAt`), the structural teeth
    /// behind the query-face's lineage-shape recognition.
    #[test]
    fn doctrine_head_shape_requires_lineage_is_current_and_created_at() {
        let contract = crate::emporium::contract::get_vocabulary("lex-scotus-core")
            .expect("lex-scotus-core registered (N1)");
        let shacl = vocab_to_shacl(contract);
        assert!(shacl.contains("urn:sophia:shacl:lex-scotus-core#DoctrineHeadShape"));
        for (curie_local, prop_path) in [
            ("lineage", "http://mnemosyne.dev/lex#lineage"),
            ("isCurrent", "http://mnemosyne.dev/lex#isCurrent"),
            ("createdAt", "http://mnemosyne.dev/lex#createdAt"),
        ] {
            let needle = format!("sh:path <{prop_path}> ;");
            let pos = shacl
                .find(&needle)
                .unwrap_or_else(|| panic!("DoctrineHead property lex:{curie_local} present"));
            let block = &shacl[pos..(pos + 200).min(shacl.len())];
            assert!(
                block.contains("sh:minCount 1"),
                "lex:{curie_local} is required on DoctrineHead: {block}"
            );
        }
    }

    /// `Holding.lex:passageAnchor` is `required: true` + `multi: true` — the
    /// pin-cite discipline (`the ontology's authority IS its citations`, per the
    /// campaign memo) has structural teeth: `sh:minCount 1`, no `sh:maxCount`.
    #[test]
    fn holding_shape_requires_passage_anchor() {
        let contract = crate::emporium::contract::get_vocabulary("lex-scotus-core")
            .expect("lex-scotus-core registered (N1)");
        let shacl = vocab_to_shacl(contract);
        // Scope the search to HOLDING'S OWN shape block. Classes emit in
        // BTreeMap order, so `DoctrinalTestShape` (alphabetically earlier)
        // precedes `HoldingShape` in the output and ALSO declares a required,
        // multi-valued `lex:passageAnchor` — searching the whole document
        // would find DoctrinalTest's property block first and pass even if
        // Holding's own passageAnchor property were dropped or changed.
        let holding_header = format!("<{}> a sh:NodeShape ;", node_shape_iri(contract, "Holding"));
        let block_start = shacl
            .find(&holding_header)
            .expect("Holding NodeShape header present");
        let after_header = block_start + holding_header.len();
        let block_end = shacl[after_header..]
            .find("a sh:NodeShape ;")
            .map(|rel| after_header + rel)
            .unwrap_or(shacl.len());
        let holding_block = &shacl[block_start..block_end];
        let needle = "sh:path <http://mnemosyne.dev/lex#passageAnchor> ;";
        let pos = holding_block
            .find(needle)
            .expect("Holding.lex:passageAnchor property present");
        let block = &holding_block[pos..(pos + 200).min(holding_block.len())];
        assert!(
            block.contains("sh:minCount 1"),
            "lex:passageAnchor is required on Holding: {block}"
        );
        assert!(
            !block.contains("sh:maxCount"),
            "lex:passageAnchor is multi-valued (pin cites can be plural): {block}"
        );
    }

    /// All eight lex-scotus-core classes carry `rdf_types` → all get a shape
    /// (parallel to memory-core's `emits_a_nodeshape_per_targetable_class`).
    #[test]
    fn lex_scotus_core_emits_a_nodeshape_per_class() {
        let contract = crate::emporium::contract::get_vocabulary("lex-scotus-core")
            .expect("lex-scotus-core registered (N1)");
        let shacl = vocab_to_shacl(contract);
        for cls in [
            "Case",
            "Justice",
            "Opinion",
            "LegalQuestion",
            "DoctrineHead",
            "Holding",
            "ReasoningStep",
            "DoctrinalTest",
        ] {
            assert!(
                shacl.contains(&format!("urn:sophia:shacl:lex-scotus-core#{cls}Shape")),
                "missing NodeShape for {cls}"
            );
        }
        assert_eq!(
            shacl.matches("a sh:NodeShape").count(),
            8,
            "exactly eight NodeShapes (one per lex-scotus-core class)"
        );
    }

    #[test]
    fn output_is_deterministic() {
        let a = vocab_to_shacl(memory_core_vocabulary());
        let b = vocab_to_shacl(memory_core_vocabulary());
        assert_eq!(
            a, b,
            "emitter is a pure, deterministic function of the contract"
        );
    }

    // ── CA-1: the contract's RAW §3 sh:sparql shapes are EMITTED into vocab_to_shacl ──

    /// A contract with NO `raw_shacl_shapes` (memory-core) emits ONLY the derived
    /// structural shapes — no `sh:sparql` leaks in (the append is a no-op when the
    /// field is `None`). The regression guard that the raw append is conditional.
    #[test]
    fn contract_without_raw_shapes_emits_no_sparql() {
        let shacl = vocab_to_shacl(memory_core_vocabulary());
        assert!(
            !shacl.contains("sh:sparql"),
            "memory-core carries no raw §3 shapes → no sh:sparql in the emitted graph"
        );
    }

    /// THE WIRING (the gap this step closes): `vocab_to_shacl(sophia-agent-core)`
    /// now emits the contract's RAW §3 shapes ALONGSIDE the derived structural ones.
    /// Before this change the §3 `sh:select` invariants (I1-membrane, I1b, I2, I2b,
    /// I3-cardinality, I4b, IdentityUnification) were inert text on the golden and
    /// never reached the validator. Here we prove they are in the emitted graph AND
    /// recover them through the REAL `sh:select` extractor (the same path the merge
    /// runs) — no mock.
    #[test]
    fn agent_core_emits_the_raw_sparql_shapes_into_the_derived_graph() {
        let c = crate::emporium::contract::get_vocabulary("sophia-agent-core")
            .expect("sophia-agent-core registered");
        let shacl = vocab_to_shacl(c);

        // The emitted graph carries the raw §3 sh:sparql constraints …
        assert!(
            shacl.contains("sh:sparql"),
            "the raw §3 shapes are appended"
        );
        // … and the SAME emitted graph still parses + extracts through the real
        // oxigraph sh:select evaluator (it is valid Turtle, not a broken concat).
        let constraints = crate::emporium::shacl_sparql::extract_sparql_constraints(&shacl)
            .expect("the merged (derived + raw §3) graph parses and extracts");
        let shape_iris: std::collections::BTreeSet<&str> =
            constraints.iter().map(|c| c.shape.as_str()).collect();
        for inv in [
            "http://mnemosyne.dev/agent#I1_MembraneWitnessShape",
            "http://mnemosyne.dev/agent#I1b_MembraneOwnerShape",
            "http://mnemosyne.dev/agent#I2_VoiceContainmentShape",
            "http://mnemosyne.dev/agent#I2b_VoiceNeedsWitnessShape",
            "http://mnemosyne.dev/agent#I3_CardinalityShape",
            "http://mnemosyne.dev/agent#I4b_SharedObserverShape",
            "http://mnemosyne.dev/agent#IdentityUnificationShape",
        ] {
            assert!(
                shape_iris.contains(inv),
                "§3 sh:select shape {inv} reaches the validator via vocab_to_shacl"
            );
        }
        // The advisories' sh:Warning severity survives the emit (the tier signal).
        assert!(
            constraints.iter().any(|c| c.severity == "Warning"),
            "the §3 advisories (I1-Valuation / I3d) emit as sh:Warning"
        );
    }
}

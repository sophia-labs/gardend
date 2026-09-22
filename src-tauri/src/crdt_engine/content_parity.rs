//! Explicit, recorded admission of content-parity imports.
//!
//! Vera conceded these on 2026-09-17, after measurement showed that of thirty
//! cloud-1 captures trialed against this engine only five imported, and that two
//! refusal classes accounted for nineteen of the twenty-five refusals.
//!
//! This is the engine-side counterpart of the collector's `content_parity`
//! module, and it keeps the same three properties, because they are what make a
//! conceded standard different from a weakened one:
//!
//! 1. **Strict is the default.** `Concessions::none()` allows nothing, and it is
//!    what every existing call site gets. An import concedes only what its own
//!    plan named.
//! 2. **A concession is per-kind and must be named.** Allowing an unresolved
//!    document identity does not allow a projection-authority conflict. An
//!    unnamed fault is still terminal, so a new defect cannot hide inside the
//!    ruling.
//! 3. **Every concession taken is recorded**, as a disposition in the import's
//!    own `retained_derived_assertions`, which already surfaces in the result as
//!    `cloud1-workspace-derived-assertion-disposition.v1`. An import admitted
//!    this way can always be told from one that passed strictly.
//!
//! What each concession gives up, and what it keeps:
//!
//! `UNRESOLVED_SOURCE_DOCUMENT_IDENTITY` — strictly, a source quad naming
//! `{source}:doc:{id}` for an id that is neither a present body nor a declared
//! unavailable one is terminal, because the rewrite cannot produce a native
//! document subject for it. Conceding retains the quad in the source-evidence
//! graph, marked reference-only, with the dangling id recorded. So the assertion
//! survives and stays queryable; what is given up is the claim that every
//! referenced document exists natively. Nothing is fabricated: no native
//! document is minted for a missing id.
//!
//! `SOURCE_ASSERTION_OUTSIDE_PROJECTION_AUTHORITY` — strictly, a source quad
//! about a projection subject using projection vocabulary is terminal, because
//! the native projection owns that field and the source may disagree with it.
//! Conceding retains the source assertion in the evidence graph alongside the
//! native statements it conflicts with, so a reader can see both. The native
//! projection stays authoritative for the live graph; the source's version is
//! preserved as testimony rather than promoted over it. That is exactly the
//! observer-relative treatment the rest of this estate uses for a disagreement.
//!
//! Both concessions require the source-evidence graph to exist. Without it there
//! is nowhere to retain the quad, and a concession that silently dropped content
//! would invert the meaning of "content parity", so the strict error stands.

use std::collections::BTreeSet;

use serde_json::Value;

pub const UNRESOLVED_SOURCE_DOCUMENT_IDENTITY: &str = "unresolved-source-document-identity";
pub const SOURCE_ASSERTION_OUTSIDE_PROJECTION_AUTHORITY: &str =
    "source-assertion-outside-projection-authority";

/// Every concession this engine knows how to take.
pub const KINDS: [&str; 2] = [
    UNRESOLVED_SOURCE_DOCUMENT_IDENTITY,
    SOURCE_ASSERTION_OUTSIDE_PROJECTION_AUTHORITY,
];

/// The ruling this module implements, recorded on every disposition it writes.
pub const RULING: &str = "vera-2026-09-17-content-parity";

/// The named tolerances one import declared. Default is none, which is strict.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Concessions {
    allowed: BTreeSet<String>,
}

impl Concessions {
    /// Strict: allows nothing. What every pre-existing call site receives.
    pub fn none() -> Self {
        Self::default()
    }

    /// Parse the `contentParity` plan field: absent or null is strict, otherwise
    /// an array of known kind strings. An unknown kind is refused rather than
    /// ignored, so a typo in a plan cannot read as "strict" and quietly hold a
    /// whole cohort, and cannot read as "everything" either.
    pub fn parse(value: Option<&Value>) -> Result<Self, String> {
        let Some(value) = value else {
            return Ok(Self::none());
        };
        if value.is_null() {
            return Ok(Self::none());
        }
        let items = value
            .as_array()
            .ok_or("graph.restoreArchive: contentParity must be an array of concession kinds")?;
        let mut allowed = BTreeSet::new();
        for item in items {
            let kind = item.as_str().ok_or(
                "graph.restoreArchive: contentParity entries must be concession kind strings",
            )?;
            if !KINDS.contains(&kind) {
                return Err(format!(
                    "graph.restoreArchive: unknown contentParity concession kind {kind}"
                ));
            }
            allowed.insert(kind.to_string());
        }
        Ok(Self { allowed })
    }

    pub fn allows(&self, kind: &str) -> bool {
        self.allowed.contains(kind)
    }

    pub fn is_strict(&self) -> bool {
        self.allowed.is_empty()
    }

    /// The declared kinds, for the envelope hash and the receipt.
    pub fn declared(&self) -> Vec<String> {
        self.allowed.iter().cloned().collect()
    }
}

/// The common half of every concession disposition, so the two call sites cannot
/// drift in how they record themselves.
pub fn disposition(
    kind: &str,
    reason: &str,
    subject: &str,
    predicate: &str,
    source_quad: &str,
    source_quad_canonical_sha256: &str,
    source_graph_iri: &str,
    source_archive_sha256: Option<&str>,
    evidence_graph: &str,
    native: Value,
) -> Value {
    serde_json::json!({
        "subject": subject,
        "predicate": predicate,
        "sourceQuad": source_quad,
        "sourceQuadCanonicalSha256": source_quad_canonical_sha256,
        "sourceGraphIri": source_graph_iri,
        "sourceArchiveSha256": source_archive_sha256,
        "source": {"quad": source_quad},
        "native": native,
        "evidenceGraph": evidence_graph,
        "authority": "captured-source-rdf-admitted-under-content-parity",
        "reason": reason,
        "disposition": "retained-queryable-source-evidence",
        "contentParity": {"ruling": RULING, "concession": kind},
        "referenceOnly": true,
        "currentEntityExistenceAsserted": false,
        "fetchAuthority": false,
        "rawSourceRetained": true
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn absent_or_null_plan_field_is_strict() {
        assert!(Concessions::parse(None).unwrap().is_strict());
        assert!(Concessions::parse(Some(&Value::Null)).unwrap().is_strict());
        assert!(!Concessions::none().allows(UNRESOLVED_SOURCE_DOCUMENT_IDENTITY));
    }

    #[test]
    fn an_empty_array_is_strict_too() {
        let parsed = Concessions::parse(Some(&json!([]))).unwrap();
        assert!(parsed.is_strict());
        assert_eq!(parsed.declared(), Vec::<String>::new());
    }

    #[test]
    fn a_named_kind_is_allowed_and_only_that_kind() {
        let parsed =
            Concessions::parse(Some(&json!([UNRESOLVED_SOURCE_DOCUMENT_IDENTITY]))).unwrap();
        assert!(parsed.allows(UNRESOLVED_SOURCE_DOCUMENT_IDENTITY));
        assert!(!parsed.allows(SOURCE_ASSERTION_OUTSIDE_PROJECTION_AUTHORITY));
        assert!(!parsed.is_strict());
        assert_eq!(parsed.declared(), vec![UNRESOLVED_SOURCE_DOCUMENT_IDENTITY]);
    }

    #[test]
    fn an_unknown_kind_is_refused_not_ignored() {
        let error = Concessions::parse(Some(&json!(["unresolved-source-document-identities"])))
            .expect_err("a typo must not read as strict or as everything");
        assert!(error.contains("unknown contentParity concession kind"), "{error}");
    }

    #[test]
    fn a_non_array_plan_field_is_refused() {
        assert!(Concessions::parse(Some(&json!("all"))).is_err());
        assert!(Concessions::parse(Some(&json!({"all": true}))).is_err());
        assert!(Concessions::parse(Some(&json!([1]))).is_err());
    }

    #[test]
    fn declared_kinds_are_sorted_and_deduplicated() {
        let parsed = Concessions::parse(Some(&json!([
            SOURCE_ASSERTION_OUTSIDE_PROJECTION_AUTHORITY,
            UNRESOLVED_SOURCE_DOCUMENT_IDENTITY,
            SOURCE_ASSERTION_OUTSIDE_PROJECTION_AUTHORITY
        ])))
        .unwrap();
        assert_eq!(
            parsed.declared(),
            vec![
                SOURCE_ASSERTION_OUTSIDE_PROJECTION_AUTHORITY,
                UNRESOLVED_SOURCE_DOCUMENT_IDENTITY
            ]
        );
    }

    #[test]
    fn a_disposition_records_the_ruling_the_concession_and_reference_only() {
        let row = disposition(
            UNRESOLVED_SOURCE_DOCUMENT_IDENTITY,
            "unresolved-source-document-identity-v1",
            "urn:example:subject",
            "urn:example:predicate",
            "<s> <p> <o> <g> .\n",
            "abc",
            "urn:mnemosyne:user:u:graph:g",
            Some("def"),
            "urn:mnemosyne:local:graph:g:user:legacy-evidence:def",
            Value::Null,
        );
        assert_eq!(row["contentParity"]["ruling"], RULING);
        assert_eq!(row["contentParity"]["concession"], UNRESOLVED_SOURCE_DOCUMENT_IDENTITY);
        assert_eq!(row["disposition"], "retained-queryable-source-evidence");
        assert_eq!(row["referenceOnly"], true);
        assert_eq!(row["currentEntityExistenceAsserted"], false);
        assert_eq!(row["rawSourceRetained"], true);
    }
}

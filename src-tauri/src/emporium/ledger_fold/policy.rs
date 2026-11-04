//! Merge policy of the ledger fold (W7 section 1, decisions 1.1 and 1.2).
//!
//! A policy answers one question: for the write an op makes to a predicate, what
//! happens when two writers disagree. It comes from the vocabulary contract
//! ([`FoldPolicy::from_contract`], reading `PredicateSpec.merge` with the default
//! rule of [`PredicateSpec::merge_policy`]) or, for a vocabulary without a contract
//! (Nidra's `ao`), from the kind of op that touches the predicate
//! ([`FoldPolicy::from_ops`]).

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use super::Op;
use crate::emporium::contract::VocabularyContract;

/// How concurrent writes to one (subject, predicate) merge.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum Merge {
    /// Functional: the highest stamp (HLC, ties by origin id, then seq) wins; the loser is
    /// lineage in the ledger, not in the head table.
    Lww,
    /// Set-valued: the union of asserted values minus the values a removal observed (a later
    /// frame of the same origin, or one that names the asserting frame as parent). A concurrent
    /// add and remove leave the value in.
    AddWins,
    /// Functional, like `Lww`, plus a loud flag when two heads are concurrent (neither names the
    /// other among its parents). With no parents supplied there is no flag, only lww.
    Contested,
}

impl Merge {
    /// The wire name (`lww`, `add-wins`, `contested`).
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Merge::Lww => "lww",
            Merge::AddWins => "add-wins",
            Merge::Contested => "contested",
        }
    }

    /// The default rule for a predicate that declares no `merge`: `add-wins` when `multi`, else
    /// `lww`.
    pub(crate) fn default_for(multi: bool) -> Merge {
        if multi {
            Merge::AddWins
        } else {
            Merge::Lww
        }
    }

    /// True for the set layer (`AddWins`); `Lww` and `Contested` fold one head per key.
    pub(crate) fn is_set(self) -> bool {
        matches!(self, Merge::AddWins)
    }

    /// Order used only to settle a predicate declared by two classes with different policies:
    /// the set reading wins, then contested, then lww.
    fn rank(self) -> u8 {
        match self {
            Merge::Lww => 0,
            Merge::Contested => 1,
            Merge::AddWins => 2,
        }
    }
}

/// The merge policy of one fold: a default, per-predicate overrides keyed by full predicate IRI,
/// and whether the op kind decides for predicates the table does not name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct FoldPolicy {
    /// Policy of a predicate the table does not name (and `by_op` does not decide).
    pub default: Merge,
    /// Per-predicate policy, keyed by the expanded predicate IRI.
    pub by_predicate: BTreeMap<String, Merge>,
    /// Contractless vocabularies (decision 1.2): for a predicate not in `by_predicate`, `set` is
    /// lww and `add`/`remove` are add-wins. The two layers of one predicate are independent.
    pub by_op: bool,
}

impl Default for FoldPolicy {
    fn default() -> Self {
        FoldPolicy {
            default: Merge::Lww,
            by_predicate: BTreeMap::new(),
            by_op: false,
        }
    }
}

impl FoldPolicy {
    /// Policy for a vocabulary with no contract: the op kind decides.
    pub(crate) fn from_ops() -> FoldPolicy {
        FoldPolicy {
            default: Merge::Lww,
            by_predicate: BTreeMap::new(),
            by_op: true,
        }
    }

    /// Policy read off a resolved contract: every predicate of every class, expanded to its full
    /// IRI, with `PredicateSpec::merge_policy()`. A predicate declared by two classes with
    /// different policies takes the stronger reading (add-wins over contested over lww), so the
    /// answer does not depend on class order. A predicate the contract does not declare folds
    /// `lww`.
    pub(crate) fn from_contract(contract: &VocabularyContract) -> FoldPolicy {
        let mut by_predicate: BTreeMap<String, Merge> = BTreeMap::new();
        for class in contract.classes.values() {
            for (key, spec) in &class.predicates {
                let Ok(uri) = contract.expand(key) else {
                    continue;
                };
                let m = spec.merge_policy();
                by_predicate
                    .entry(uri)
                    .and_modify(|cur| {
                        if m.rank() > cur.rank() {
                            *cur = m;
                        }
                    })
                    .or_insert(m);
            }
        }
        FoldPolicy {
            default: Merge::Lww,
            by_predicate,
            by_op: false,
        }
    }

    /// The merge that applies to `op` writing `predicate`.
    pub(crate) fn merge_for(&self, predicate: &str, op: &Op) -> Merge {
        if let Some(m) = self.by_predicate.get(predicate) {
            return *m;
        }
        if self.by_op {
            return match op {
                Op::Add { .. } | Op::Remove { .. } => Merge::AddWins,
                Op::Set { .. } | Op::Retract { .. } => Merge::Lww,
            };
        }
        self.default
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::emporium::contract::PredicateSpec;
    use serde_json::json;

    fn spec(v: serde_json::Value) -> PredicateSpec {
        serde_json::from_value(v).expect("predicate spec parses")
    }

    #[test]
    fn default_rule_is_add_wins_when_multi_else_lww() {
        assert_eq!(
            spec(json!({"datatype": "string"})).merge_policy(),
            Merge::Lww
        );
        assert_eq!(
            spec(json!({"datatype": "string", "multi": true})).merge_policy(),
            Merge::AddWins
        );
        assert_eq!(Merge::default_for(true), Merge::AddWins);
        assert_eq!(Merge::default_for(false), Merge::Lww);
    }

    #[test]
    fn declared_merge_overrides_the_default_rule() {
        assert_eq!(
            spec(json!({"datatype": "string", "merge": "contested"})).merge_policy(),
            Merge::Contested
        );
        assert_eq!(
            spec(json!({"datatype": "string", "multi": true, "merge": "lww"})).merge_policy(),
            Merge::Lww
        );
        assert_eq!(
            spec(json!({"datatype": "string", "merge": "add-wins"})).merge_policy(),
            Merge::AddWins
        );
    }

    #[test]
    fn unknown_merge_name_is_a_parse_error() {
        let r: Result<PredicateSpec, _> =
            serde_json::from_value(json!({"datatype": "string", "merge": "last-one-wins"}));
        assert!(r.is_err());
    }

    #[test]
    fn merge_names_round_trip() {
        for m in [Merge::Lww, Merge::AddWins, Merge::Contested] {
            let s = serde_json::to_string(&m).unwrap();
            assert_eq!(s, format!("\"{}\"", m.as_str()));
            assert_eq!(serde_json::from_str::<Merge>(&s).unwrap(), m);
        }
    }

    fn contract() -> VocabularyContract {
        serde_json::from_value(json!({
            "name": "t",
            "version": "1.0.0",
            "title": "T",
            "description": "policy test",
            "namespaces": {"t": "urn:t:"},
            "primary_prefix": "t",
            "classes": {
                "A": {"predicates": {
                    "t:name": {"datatype": "string"},
                    "t:tags": {"datatype": "string", "multi": true},
                    "t:owner": {"datatype": "uri", "merge": "contested"}
                }},
                "B": {"predicates": {
                    "t:name": {"datatype": "string", "multi": true}
                }}
            }
        }))
        .expect("contract parses")
    }

    #[test]
    fn from_contract_reads_every_predicate_and_settles_disagreements() {
        let p = FoldPolicy::from_contract(&contract());
        assert_eq!(p.by_predicate.get("urn:t:tags"), Some(&Merge::AddWins));
        assert_eq!(p.by_predicate.get("urn:t:owner"), Some(&Merge::Contested));
        // class A says lww, class B says add-wins: the set reading wins whatever the class order
        assert_eq!(p.by_predicate.get("urn:t:name"), Some(&Merge::AddWins));
        assert!(!p.by_op);
        let set = Op::Set {
            subject: "urn:s".into(),
            class: "urn:t:A".into(),
            predicate: "urn:t:unknown".into(),
            value: None,
        };
        assert_eq!(p.merge_for("urn:t:unknown", &set), Merge::Lww);
        assert_eq!(p.merge_for("urn:t:tags", &set), Merge::AddWins);
    }

    #[test]
    fn from_ops_lets_the_op_kind_decide() {
        let p = FoldPolicy::from_ops();
        let set = Op::Set {
            subject: "urn:s".into(),
            class: "urn:c".into(),
            predicate: "urn:p".into(),
            value: None,
        };
        let add = Op::Add {
            subject: "urn:s".into(),
            class: "urn:c".into(),
            predicate: "urn:p".into(),
            values: vec![],
        };
        let rem = Op::Remove {
            subject: "urn:s".into(),
            class: "urn:c".into(),
            predicate: "urn:p".into(),
            values: vec![],
        };
        assert_eq!(p.merge_for("urn:p", &set), Merge::Lww);
        assert_eq!(p.merge_for("urn:p", &add), Merge::AddWins);
        assert_eq!(p.merge_for("urn:p", &rem), Merge::AddWins);
    }
}

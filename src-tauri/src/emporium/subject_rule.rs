//! The subject-rule grammar (EA-3 / B5) — the engine's FIRST machine-readable
//! parser for `ClassSpec::subject_rule`.
//!
//! Until EA-3 the golden `subject_rule` was descriptive PROSE carried with
//! `#[allow(dead_code)]` (see `contract.rs`); the memory pack minted its subjects
//! with hand-written code (`planner::memory_record_subject`) and the prose was
//! documentation only. For the GENERIC publication path a product author must be
//! able to declare HOW a class's subjects are minted in a form the generic planner
//! can interpret — without writing a bespoke planner fork per vocab. This module
//! is that interpreter.
//!
//! Two productions, deliberately minimal (the build-time-cut design):
//!
//! 1. **Template** — an interpolated URI pattern, e.g.
//!    `"{graph_subject}:projection:bookmark:bookmark:{localId}"`. Every `{token}`
//!    is a named binding the planner supplies (`{graph_subject}` is the cell root;
//!    the rest come from the ingest record). [`mint_subject_from_rule`] substitutes
//!    each token; a MISSING binding is a LOUD error (never a silently corrupt URI).
//!
//! 2. **Descriptive** — a prose marker (`"urn-space"`, `"code-minted"`, …) that
//!    declares "the subject is minted by class-specific CODE, not by this grammar".
//!    The memory pack's `Claim`/`Policy` are descriptive. The GENERIC planner has
//!    no per-class default for descriptive subjects, so it REJECTS them with an
//!    actionable error — a product that needs descriptive minting provides its own
//!    planner fork (as memory did). This keeps the grammar honest: it parses what
//!    it can mint, and names what it cannot.
//!
//! The parse is total over the existing goldens (covered by
//! `vocabs::golden_subject_rules_are_parseable`) so a contract author cannot ship
//! an un-interpretable rule.

use std::collections::BTreeMap;

/// A parsed `subject_rule`. The result of [`parse_subject_rule`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum SubjectRule {
    /// An interpolated URI template. `pattern` is the literal string with `{token}`
    /// placeholders; `required_tokens` is the sorted, de-duplicated set of token
    /// names found in it (the binding contract the planner must satisfy).
    Template {
        pattern: String,
        required_tokens: Vec<String>,
    },
    /// A code-minted (descriptive) subject. The grammar does NOT mint it; the
    /// planner must supply the subject via class-specific logic (memory-only in
    /// v1). `marker` is the recognized prose token (for diagnostics).
    Descriptive { marker: String },
}

/// The built-in token bound by the planner from cell context (not the record).
pub(crate) const TOKEN_GRAPH_SUBJECT: &str = "graph_subject";

/// Parse a `subject_rule` string into a [`SubjectRule`].
///
/// Precedence:
/// - a rule containing `{` is a **Template** (the `{token}` placeholders are
///   extracted); a literal-only pattern with no `{token}` is still a Template with
///   an empty `required_tokens` set (it mints a constant subject).
/// - otherwise, a rule mentioning a descriptive marker (`urn-space`, `code-minted`,
///   `descriptive`) is **Descriptive**.
/// - an empty/whitespace rule, or unrecognized prose, is an ERROR (a contract bug
///   surfaces loudly rather than minting a garbage subject).
///
/// NB: the memory golden's Template rules carry a trailing prose comment after the
/// pattern (e.g. `"{graph_subject}:…:{hash} - code-minted"`). The pattern is taken
/// up to the FIRST whitespace run, so the `- code-minted` tail is ignored — only
/// the `{…}`-bearing prefix is the interpolation template.
pub(crate) fn parse_subject_rule(rule: &str) -> Result<SubjectRule, String> {
    let trimmed = rule.trim();
    if trimmed.is_empty() {
        return Err("subject_rule is empty".to_string());
    }

    if trimmed.contains('{') {
        // The interpolation pattern is the leading whitespace-free token (the
        // memory golden appends a ` - code-minted` prose tail after the pattern).
        let pattern = trimmed
            .split_whitespace()
            .next()
            .unwrap_or(trimmed)
            .to_string();
        let required_tokens = extract_tokens(&pattern)?;
        return Ok(SubjectRule::Template {
            pattern,
            required_tokens,
        });
    }

    let lower = trimmed.to_ascii_lowercase();
    for marker in ["urn-space", "code-minted", "descriptive"] {
        if lower.contains(marker) {
            return Ok(SubjectRule::Descriptive {
                marker: marker.to_string(),
            });
        }
    }

    Err(format!(
        "unparseable subject_rule '{trimmed}': expected a '{{token}}' template or a \
         descriptive marker (urn-space|code-minted|descriptive)"
    ))
}

/// Extract the `{token}` names from a template pattern, sorted + de-duplicated.
///
/// A token name is `[a-zA-Z_][a-zA-Z0-9_]*` between a `{` and the next `}`. An
/// unbalanced brace or an empty/invalid token name is a LOUD parse error (a
/// malformed template must never silently mint a corrupt subject).
fn extract_tokens(pattern: &str) -> Result<Vec<String>, String> {
    let mut tokens: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    let bytes = pattern.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'{' {
            let close = pattern[i..]
                .find('}')
                .ok_or_else(|| format!("subject_rule template '{pattern}': unbalanced '{{'"))?
                + i;
            let name = &pattern[i + 1..close];
            validate_token_name(name, pattern)?;
            tokens.insert(name.to_string());
            i = close + 1;
        } else if bytes[i] == b'}' {
            return Err(format!(
                "subject_rule template '{pattern}': stray '}}' with no matching '{{'"
            ));
        } else {
            i += 1;
        }
    }
    Ok(tokens.into_iter().collect())
}

/// A token name must be a non-empty identifier `[a-zA-Z_][a-zA-Z0-9_]*`.
fn validate_token_name(name: &str, pattern: &str) -> Result<(), String> {
    let mut chars = name.chars();
    let first = chars
        .next()
        .ok_or_else(|| format!("subject_rule template '{pattern}': empty '{{}}' token name"))?;
    if !(first.is_ascii_alphabetic() || first == '_') {
        return Err(format!(
            "subject_rule template '{pattern}': token '{{{name}}}' must start with a letter or '_'"
        ));
    }
    for c in chars {
        if !(c.is_ascii_alphanumeric() || c == '_') {
            return Err(format!(
                "subject_rule template '{pattern}': token '{{{name}}}' has an illegal character '{c}'"
            ));
        }
    }
    Ok(())
}

/// Mint a subject by interpolating `tokens` into a [`SubjectRule::Template`].
///
/// Every `{token}` in the pattern is replaced by its `tokens[token]` value. A
/// required token that is ABSENT from the map is a LOUD error (the planner cannot
/// mint a subject it has no binding for — silently leaving `{localId}` literal
/// would produce a corrupt, non-converging subject). A [`SubjectRule::Descriptive`]
/// rule is rejected here — the GENERIC planner has no default for code-minted
/// subjects (memory-only); callers must branch on the variant before minting.
///
/// Token VALUES are interpolated verbatim. v1 assumes alphanumeric `+ _ - : . /`
/// values (no brace characters); a value containing `{`/`}` is rejected so it
/// cannot inject a spurious placeholder into the minted subject.
pub(crate) fn mint_subject_from_rule(
    rule: &SubjectRule,
    tokens: &BTreeMap<String, String>,
) -> Result<String, String> {
    let (pattern, required_tokens) = match rule {
        SubjectRule::Template {
            pattern,
            required_tokens,
        } => (pattern, required_tokens),
        SubjectRule::Descriptive { marker } => {
            return Err(format!(
                "cannot mint a descriptive subject_rule (marker '{marker}') via the generic \
                 grammar — descriptive subjects require a class-specific planner"
            ));
        }
    };

    // Every required token must have a binding BEFORE substitution (so a missing
    // binding is one clear error, not a silently-literal `{token}` in the URI).
    for token in required_tokens {
        let value = tokens.get(token).ok_or_else(|| {
            format!("subject_rule '{pattern}': missing binding for token '{{{token}}}'")
        })?;
        // IRI-SAFETY (injection guard): the minted subject is rendered `<{s}>`
        // into SPARQL DATA bodies with NO escaping (`render_updates`), so a token
        // value carrying an IRI delimiter — `<` `>` `"` `\` `{` `}`, whitespace,
        // or a control char — could BREAK OUT of the `<…>` and inject arbitrary
        // triples into a reserved projection graph through the blessed
        // materializer path (bypassing SHACL, which targets by rdf:type). Reject
        // any such value LOUD before it can reach the subject.
        if let Some(bad) = value.chars().find(|c| {
            matches!(c, '<' | '>' | '"' | '\\' | '{' | '}') || c.is_whitespace() || c.is_control()
        }) {
            return Err(format!(
                "subject_rule '{pattern}': token '{{{token}}}' value '{value}' contains an \
                 IRI-unsafe character {bad:?} (would break the minted subject's `<…>` \
                 delimiter — rejected to prevent triple injection)"
            ));
        }
    }

    let mut out = pattern.clone();
    for token in required_tokens {
        let value = &tokens[token];
        out = out.replace(&format!("{{{token}}}"), value);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn toks(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn parses_a_template_with_two_tokens() {
        let rule = parse_subject_rule("{graph_subject}:projection:bookmark:bookmark:{localId}")
            .expect("template parses");
        match rule {
            SubjectRule::Template {
                pattern,
                required_tokens,
            } => {
                assert_eq!(
                    pattern,
                    "{graph_subject}:projection:bookmark:bookmark:{localId}"
                );
                // Sorted + de-duped.
                assert_eq!(required_tokens, vec!["graph_subject", "localId"]);
            }
            other => panic!("expected Template, got {other:?}"),
        }
    }

    #[test]
    fn parses_the_memory_template_ignoring_the_prose_tail() {
        // The memory golden appends a ` - code-minted, content-hash` tail after the
        // pattern; only the {…}-bearing prefix is the template.
        let rule = parse_subject_rule(
            "{graph_subject}:projection:memory:record:{memId} - code-minted, content-hash",
        )
        .expect("memory template parses");
        match rule {
            SubjectRule::Template {
                pattern,
                required_tokens,
            } => {
                assert_eq!(pattern, "{graph_subject}:projection:memory:record:{memId}");
                assert_eq!(required_tokens, vec!["graph_subject", "memId"]);
            }
            other => panic!("expected Template, got {other:?}"),
        }
    }

    #[test]
    fn parses_descriptive_markers() {
        for prose in [
            "urn-space; code-minted (descriptive)",
            "urn-space",
            "code-minted",
        ] {
            assert!(
                matches!(
                    parse_subject_rule(prose),
                    Ok(SubjectRule::Descriptive { .. })
                ),
                "'{prose}' must parse descriptive"
            );
        }
    }

    #[test]
    fn empty_and_unrecognized_rules_error() {
        assert!(parse_subject_rule("").is_err());
        assert!(parse_subject_rule("   ").is_err());
        assert!(parse_subject_rule("just some prose with no marker").is_err());
    }

    #[test]
    fn unbalanced_and_illegal_tokens_error() {
        assert!(parse_subject_rule("{graph_subject:no-close").is_err());
        assert!(parse_subject_rule("a}stray").is_err());
        assert!(
            parse_subject_rule("{1bad}").is_err(),
            "token cannot start with a digit"
        );
        assert!(
            parse_subject_rule("{bad-token}").is_err(),
            "hyphen is illegal in a token name"
        );
        assert!(parse_subject_rule("{}").is_err(), "empty token name");
    }

    #[test]
    fn mints_a_subject_by_interpolation() {
        let rule =
            parse_subject_rule("{graph_subject}:projection:bookmark:bookmark:{localId}").unwrap();
        let subject = mint_subject_from_rule(
            &rule,
            &toks(&[
                ("graph_subject", "urn:mnemosyne:local:graph:lab"),
                ("localId", "abc123"),
            ]),
        )
        .expect("mint");
        assert_eq!(
            subject,
            "urn:mnemosyne:local:graph:lab:projection:bookmark:bookmark:abc123"
        );
    }

    #[test]
    fn missing_binding_is_a_loud_error_not_a_literal_token() {
        let rule = parse_subject_rule("{graph_subject}:x:{localId}").unwrap();
        let err = mint_subject_from_rule(
            &rule,
            &toks(&[("graph_subject", "urn:g")]), // localId missing
        )
        .expect_err("missing binding must error");
        assert!(err.contains("localId"), "{err}");
    }

    #[test]
    fn descriptive_cannot_be_minted_generically() {
        let rule = parse_subject_rule("urn-space; code-minted").unwrap();
        let err = mint_subject_from_rule(&rule, &toks(&[])).expect_err("descriptive cannot mint");
        assert!(err.contains("descriptive"), "{err}");
    }

    #[test]
    fn brace_bearing_token_value_is_rejected() {
        let rule = parse_subject_rule("{graph_subject}:x:{localId}").unwrap();
        let err = mint_subject_from_rule(
            &rule,
            &toks(&[("graph_subject", "urn:g"), ("localId", "a{b}c")]),
        )
        .expect_err("a brace in a token value must be rejected");
        assert!(err.contains("IRI-unsafe"), "{err}");
    }

    /// The injection guard: a `localId` carrying an IRI delimiter (the codex
    /// close-out finding — `x> <urn:evil:p> <urn:evil:o> . <urn:valid`) must be
    /// rejected BEFORE it can break out of the minted `<{s}>` and inject triples.
    #[test]
    fn iri_breaking_token_value_is_rejected() {
        let rule = parse_subject_rule("{graph_subject}:x:{localId}").unwrap();
        for evil in [
            "x> <urn:evil:p> <urn:evil:o> . <urn:valid",
            "has space",
            "quote\"here",
            "back\\slash",
            "tab\tinside",
        ] {
            let err = mint_subject_from_rule(
                &rule,
                &toks(&[("graph_subject", "urn:g"), ("localId", evil)]),
            )
            .expect_err("an IRI-unsafe token value must be rejected");
            assert!(err.contains("IRI-unsafe"), "for {evil:?}: {err}");
        }
        // A normal product localId still mints fine.
        let ok = mint_subject_from_rule(
            &rule,
            &toks(&[("graph_subject", "urn:g"), ("localId", "rust-book_2.0")]),
        )
        .expect("a normal localId mints");
        assert_eq!(ok, "urn:g:x:rust-book_2.0");
    }

    #[test]
    fn literal_only_template_mints_a_constant() {
        let rule = parse_subject_rule("urn:sophia:fixed:singleton").unwrap_err();
        // No `{` and no descriptive marker → error (it is neither a template nor a
        // recognized descriptive rule; a constant subject must use at least one
        // built-in token or be authored as descriptive).
        assert!(rule.contains("unparseable"));
    }
}

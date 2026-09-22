pub(crate) fn predicate_short_name(predicate: &str) -> String {
    predicate
        .rsplit(['#', '/'])
        .next()
        .filter(|value| !value.is_empty())
        .unwrap_or(predicate)
        .to_string()
}

pub(crate) fn builtin_wire_predicates() -> Vec<(&'static str, &'static str, &'static str)> {
    vec![
        ("isWiredTo", "is wired to", ""),
        ("partOf", "is part of", "Quantity"),
        ("contains", "contains", "Quantity"),
        ("exemplifies", "is an example of", "Quantity"),
        ("supports", "supports", "Quality"),
        ("contradicts", "contradicts", "Quality"),
        ("qualifies", "qualifies", "Quality"),
        ("causeOf", "causes", "Relation"),
        ("consequenceOf", "is consequence of", "Relation"),
        ("relatedTo", "is related to", "Relation"),
        ("requires", "requires", "Modality"),
        ("enables", "enables", "Modality"),
        ("precedes", "precedes", "Modality"),
        ("flowsInto", "flows into", "Synthesis"),
        ("produces", "produces", "Synthesis"),
        ("divergesFrom", "diverges from", "Synthesis"),
        ("branchesTo", "branches to", "Synthesis"),
        ("consumesWith", "consumes with", "Synthesis"),
        ("intensifiesWith", "intensifies with", "Synthesis"),
    ]
}

fn legacy_wire_predicate_label(short_name: &str) -> Option<&'static str> {
    match short_name {
        "isWiredTo" => Some("is wired to"),
        "causeOf" => Some("causes"),
        "partOf" => Some("is part of"),
        "divergesFrom" => Some("diverges from"),
        "branchesTo" => Some("branches to"),
        "consumesWith" => Some("consumes with"),
        "synthesizes" => Some("synthesizes"),
        _ => None,
    }
}

pub(crate) fn predicate_label(predicate: &str) -> String {
    let short_name = predicate_short_name(predicate);
    builtin_wire_predicates()
        .into_iter()
        .find(|(name, _, _)| *name == short_name)
        .map(|(_, label, _)| label.to_string())
        .or_else(|| legacy_wire_predicate_label(&short_name).map(str::to_string))
        .unwrap_or_else(|| short_name.clone())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn predicate_helpers_preserve_builtin_legacy_and_uri_labels() {
        assert_eq!(
            predicate_short_name("http://mnemosyne.ai/vocab#supports"),
            "supports"
        );
        assert_eq!(predicate_short_name("urn:custom/predicate"), "predicate");
        assert_eq!(predicate_label("supports"), "supports");
        assert_eq!(predicate_label("causeOf"), "causes");
        assert_eq!(predicate_label("synthesizes"), "synthesizes");
        assert_eq!(predicate_label("customRelation"), "customRelation");
    }
}

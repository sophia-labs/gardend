use crate::{
    loopback_knowledge_scopes::LOOPBACK_KNOWLEDGE_SCOPES,
    loopback_system_scopes::LOOPBACK_SYSTEM_SCOPES,
    loopback_workspace_scopes::LOOPBACK_WORKSPACE_SCOPES,
};

pub(crate) use crate::loopback_scope_types::LoopbackScopeDetail;

const LOOPBACK_SCOPE_GROUPS: &[&[LoopbackScopeDetail]] = &[
    LOOPBACK_SYSTEM_SCOPES,
    LOOPBACK_WORKSPACE_SCOPES,
    LOOPBACK_KNOWLEDGE_SCOPES,
];

pub(crate) fn loopback_scope_keys() -> Vec<&'static str> {
    LOOPBACK_SCOPE_GROUPS
        .iter()
        .flat_map(|group| group.iter().map(LoopbackScopeDetail::key))
        .collect()
}

pub(crate) fn loopback_scope_details() -> Vec<LoopbackScopeDetail> {
    LOOPBACK_SCOPE_GROUPS
        .iter()
        .flat_map(|group| {
            group
                .iter()
                .copied()
                .map(|detail| detail.with_default_grant(loopback_scope_default_grant(&detail)))
        })
        .collect()
}

fn loopback_scope_default_grant(detail: &LoopbackScopeDetail) -> bool {
    detail.access() == "read"
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    #[test]
    fn loopback_scope_keys_are_unique_and_explicit() {
        let keys = loopback_scope_keys();
        let unique = keys.iter().copied().collect::<BTreeSet<_>>();

        assert_eq!(unique.len(), keys.len());
        assert!(unique.contains("documents.write.crdt"));
        assert!(unique.contains("documents.delete.crdt"));
        assert!(unique.contains("workspace.write.crdt"));
        assert!(unique.contains("workspace.delete.crdt"));
        assert!(unique.contains("artifacts.ingest"));
        assert!(unique.contains("rdf.query"));
        assert!(unique.contains("semantic.index.cancel"));
        assert!(unique.contains("mcp.tools.call"));
    }

    #[test]
    fn loopback_scope_default_grants_are_read_only() {
        let details = loopback_scope_details();

        assert!(details
            .iter()
            .filter(|detail| detail.access() == "read")
            .all(LoopbackScopeDetail::default_grant));
        assert!(details
            .iter()
            .filter(|detail| detail.access() != "read")
            .all(|detail| !detail.default_grant()));
    }
}

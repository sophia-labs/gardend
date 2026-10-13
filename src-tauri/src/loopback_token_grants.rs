use crate::loopback_scope_catalog::{
    loopback_scope_details, loopback_scope_keys, LoopbackScopeDetail,
};
use serde::Serialize;
use std::collections::BTreeSet;

pub(crate) const TOKEN_SCOPE_MODE: &str = "session-all";

#[derive(Debug, Clone)]
pub(crate) struct LoopbackTokenGrant {
    pub(crate) scope_mode: &'static str,
    pub(crate) scopes: Vec<&'static str>,
    pub(crate) scope_details: Vec<LoopbackScopeDetail>,
    pub(crate) grant_profiles: Vec<LoopbackGrantProfile>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct LoopbackGrantProfile {
    pub(crate) id: &'static str,
    pub(crate) label: &'static str,
    pub(crate) description: &'static str,
    pub(crate) scopes: Vec<&'static str>,
    pub(crate) default_grant: bool,
    pub(crate) mutating: bool,
}

pub(crate) fn session_all_token_grant() -> LoopbackTokenGrant {
    let scopes = loopback_scope_keys();
    let scope_details = loopback_scope_details();
    LoopbackTokenGrant {
        scope_mode: TOKEN_SCOPE_MODE,
        grant_profiles: loopback_grant_profiles(&scopes, &scope_details),
        scopes,
        scope_details,
    }
}

/// The member grant: what a person or agent working in a Sophia needs, without
/// administering it. Every read scope, the `authoring` writes, and MCP calls
/// (each tool still checks its own scopes). Not: deletes, creating, importing
/// or restoring graphs, loading or updating RDF, memory and index writes,
/// time-travel writes, service, model and token administration. A self-host's
/// Cloudflare Access members get it (`loopback_self_host`), and
/// `gardend token create --grant member` mints it for agents.
pub(crate) const MEMBER_GRANT_PROFILE: &str = "member";

const AUTHORING_SCOPES: &[&str] = &[
    "graphs.read",
    "documents.read",
    "documents.history.read",
    "documents.write.crdt",
    "documents.snapshots.write",
    "workspace.read",
    "workspace.write.crdt",
    "artifacts.read",
    "artifacts.write",
    "artifacts.ingest",
    "images.read",
    "images.write",
    "search.lexical.read",
    "wires.read",
    "wires.write",
    "entities.read",
    "entities.write",
    "imports.web.write",
];

fn loopback_grant_profiles(
    all_scopes: &[&'static str],
    scope_details: &[LoopbackScopeDetail],
) -> Vec<LoopbackGrantProfile> {
    let read_scopes: Vec<&'static str> = scope_details
        .iter()
        .filter(|detail| detail.access() == "read")
        .map(LoopbackScopeDetail::key)
        .collect();
    let authoring_scopes = selected_scopes(all_scopes, AUTHORING_SCOPES);
    let member_scopes: Vec<&'static str> = read_scopes
        .iter()
        .chain(authoring_scopes.iter())
        .copied()
        .chain(selected_scopes(all_scopes, &["mcp.tools.call"]))
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    vec![
        LoopbackGrantProfile {
            id: "read-only",
            label: "Read-only",
            description: "Read local graphs, documents, workspace projections, RDF, search, runtime status, and diagnostics.",
            scopes: read_scopes,
            default_grant: true,
            mutating: false,
        },
        LoopbackGrantProfile {
            id: "authoring",
            label: "Authoring",
            description: "Read and write documents, workspace structure, artifacts, inline images, wires, and web imports without delete/admin scopes.",
            scopes: authoring_scopes,
            default_grant: false,
            mutating: true,
        },
        LoopbackGrantProfile {
            id: MEMBER_GRANT_PROFILE,
            label: "Member",
            description: "Every read, the authoring writes, and MCP tool calls: work in every graph without deleting, creating or restoring graphs, loading or updating RDF, or administering the runtime.",
            scopes: member_scopes,
            default_grant: false,
            mutating: true,
        },
        LoopbackGrantProfile {
            id: "rdf-admin",
            label: "RDF admin",
            description: "Read, load, update, and dump local RDF graphs.",
            scopes: selected_scopes(all_scopes, &["rdf.query", "rdf.dump", "rdf.load", "rdf.update"]),
            default_grant: false,
            mutating: true,
        },
        LoopbackGrantProfile {
            id: "session-all",
            label: "Session all",
            description: "Compatibility grant containing every known local loopback and MCP scope.",
            scopes: all_scopes.to_vec(),
            default_grant: false,
            mutating: true,
        },
    ]
}

fn selected_scopes(all_scopes: &[&'static str], requested: &[&'static str]) -> Vec<&'static str> {
    let available = all_scopes.iter().copied().collect::<BTreeSet<_>>();
    requested
        .iter()
        .copied()
        .filter(|scope| available.contains(scope))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    #[test]
    fn session_all_grant_includes_public_contract_surfaces_once() {
        let grant = session_all_token_grant();
        let unique = grant.scopes.iter().copied().collect::<BTreeSet<_>>();

        assert_eq!(grant.scope_mode, "session-all");
        assert_eq!(unique.len(), grant.scopes.len());
        assert!(unique.contains("loopback.manifest.read"));
        assert!(unique.contains("documents.write.crdt"));
        assert!(unique.contains("documents.delete.crdt"));
        assert!(unique.contains("workspace.write.crdt"));
        assert!(unique.contains("workspace.delete.crdt"));
        assert!(unique.contains("rdf.query"));
        assert!(unique.contains("semantic.index.write"));
        assert!(unique.contains("mcp.tools.call"));
    }

    #[test]
    fn session_all_grant_scope_details_cover_granted_scopes() {
        let grant = session_all_token_grant();
        let detail_scopes = grant
            .scope_details
            .iter()
            .map(LoopbackScopeDetail::key)
            .collect::<BTreeSet<_>>();

        assert_eq!(
            detail_scopes,
            grant.scopes.iter().copied().collect::<BTreeSet<_>>()
        );
    }

    #[test]
    fn grant_profiles_are_unique_and_include_compatibility_profile() {
        let grant = session_all_token_grant();
        let ids = grant
            .grant_profiles
            .iter()
            .map(|profile| profile.id)
            .collect::<BTreeSet<_>>();

        assert_eq!(ids.len(), grant.grant_profiles.len());
        assert!(ids.contains("read-only"));
        assert!(ids.contains("authoring"));
        assert!(ids.contains("rdf-admin"));
        assert!(ids.contains("session-all"));
        assert!(ids.contains(MEMBER_GRANT_PROFILE));
    }

    #[test]
    fn member_profile_is_reads_authoring_writes_and_mcp_calls() {
        let grant = session_all_token_grant();
        let profile = |id: &str| {
            grant
                .grant_profiles
                .iter()
                .find(|profile| profile.id == id)
                .unwrap_or_else(|| panic!("{id} profile"))
                .scopes
                .iter()
                .copied()
                .collect::<BTreeSet<_>>()
        };
        let member = profile(MEMBER_GRANT_PROFILE);
        let mut expected = profile("read-only");
        expected.extend(profile("authoring"));
        expected.insert("mcp.tools.call");
        assert_eq!(member, expected);
        for admin_only in [
            "documents.delete.crdt",
            "workspace.delete.crdt",
            "graphs.write",
            "rdf.load",
            "rdf.update",
            "semantic.index.write",
        ] {
            assert!(!member.contains(admin_only), "a member lacks {admin_only}");
        }
    }

    #[test]
    fn read_only_profile_uses_all_and_only_read_scopes() {
        let grant = session_all_token_grant();
        let read_scopes = grant
            .scope_details
            .iter()
            .filter(|detail| detail.access() == "read")
            .map(LoopbackScopeDetail::key)
            .collect::<BTreeSet<_>>();
        let read_only_profile = grant
            .grant_profiles
            .iter()
            .find(|profile| profile.id == "read-only")
            .expect("read-only profile");

        assert!(read_only_profile.default_grant);
        assert!(!read_only_profile.mutating);
        assert_eq!(
            read_only_profile
                .scopes
                .iter()
                .copied()
                .collect::<BTreeSet<_>>(),
            read_scopes
        );
    }

    #[test]
    fn mutating_grant_profiles_are_not_default_granted() {
        let grant = session_all_token_grant();
        let known_scopes = grant.scopes.iter().copied().collect::<BTreeSet<_>>();

        for profile in grant
            .grant_profiles
            .iter()
            .filter(|profile| profile.mutating)
        {
            assert!(!profile.default_grant, "{} must need consent", profile.id);
            assert!(
                profile
                    .scopes
                    .iter()
                    .all(|scope| known_scopes.contains(scope)),
                "{} must reference only known scopes",
                profile.id
            );
        }
    }
}

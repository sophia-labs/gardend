use crate::loopback_scope_types::{scope, LoopbackScopeDetail};

pub(crate) const LOOPBACK_SYSTEM_SCOPES: &[LoopbackScopeDetail] = &[
    scope(
        "loopback.health.read",
        "loopback",
        "read",
        "Read loopback health status.",
    ),
    scope(
        "loopback.manifest.read",
        "loopback",
        "read",
        "Read the loopback connection manifest.",
    ),
    scope(
        "loopback.openapi.read",
        "loopback",
        "read",
        "Read the generated local OpenAPI document.",
    ),
    scope(
        "loopback.tokens.read",
        "loopback",
        "read",
        "Read named loopback client token metadata.",
    ),
    scope(
        "loopback.tokens.write",
        "loopback",
        "write",
        "Issue named scoped loopback client tokens.",
    ),
    scope(
        "loopback.tokens.delete",
        "loopback",
        "delete",
        "Revoke named loopback client tokens.",
    ),
    scope(
        "runtime.capabilities.read",
        "runtime",
        "read",
        "Read local runtime capability descriptors.",
    ),
    scope(
        "services.read",
        "services",
        "read",
        "Read local hosted-service status and logs.",
    ),
    scope(
        "services.manage",
        "services",
        "write",
        "Start and stop local hosted services.",
    ),
    scope(
        "services.proxy",
        "services",
        "write",
        "Proxy requests to local hosted services.",
    ),
    scope(
        "profile.read",
        "profile",
        "read",
        "Read local profile metadata.",
    ),
    scope(
        "diagnostics.crdt-timings.read",
        "diagnostics",
        "read",
        "Read local CRDT operation timing traces.",
    ),
    scope(
        "graphs.read",
        "graphs",
        "read",
        "Read local graph metadata.",
    ),
    scope(
        "graphs.write",
        "graphs",
        "write",
        "Create or update local graphs.",
    ),
    scope("graphs.delete", "graphs", "delete", "Delete local graphs."),
    scope(
        "graphs.export",
        "graphs",
        "read",
        "Export local graph data.",
    ),
    scope(
        "graphs.import",
        "graphs",
        "write",
        "Import local graph data.",
    ),
    scope(
        "graphs.restore",
        "graphs",
        "write",
        "Restore a bound archive into an empty existing single-graph cell.",
    ),
    scope(
        "jobs.read",
        "jobs",
        "read",
        "Read local job status and result records.",
    ),
    scope("jobs.cancel", "jobs", "delete", "Cancel local jobs."),
];

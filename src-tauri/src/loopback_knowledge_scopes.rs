use crate::loopback_scope_types::{scope, LoopbackScopeDetail};

pub(crate) const LOOPBACK_KNOWLEDGE_SCOPES: &[LoopbackScopeDetail] = &[
    scope(
        "semantic.models.read",
        "semantic",
        "read",
        "Read local semantic model status.",
    ),
    scope(
        "semantic.models.write",
        "semantic",
        "write",
        "Configure or prepare local semantic models.",
    ),
    scope(
        "semantic.index.read",
        "semantic",
        "read",
        "Read local semantic index status and refresh job state.",
    ),
    scope(
        "semantic.index.write",
        "semantic",
        "write",
        "Refresh or rematerialize local indexes and projections.",
    ),
    scope(
        "semantic.index.cancel",
        "semantic",
        "delete",
        "Cancel local semantic index refresh jobs.",
    ),
    scope(
        "ingestion.config.read",
        "ingestion",
        "read",
        "Read local ingestion pipeline configuration.",
    ),
    scope(
        "ingestion.config.write",
        "ingestion",
        "write",
        "Update local ingestion pipeline configuration.",
    ),
    scope(
        "orientation.read",
        "orientation",
        "read",
        "Read local orientation bundles.",
    ),
    scope(
        "memory.read",
        "memory",
        "read",
        "Read local memory and Song data.",
    ),
    scope(
        "memory.write",
        "memory",
        "write",
        "Write local memory and Song data.",
    ),
    scope(
        "salience.read",
        "salience",
        "read",
        "Read local valuation and salience data.",
    ),
    scope(
        "salience.write",
        "salience",
        "write",
        "Write local valuation and salience data.",
    ),
    scope(
        "sources.read",
        "sources",
        "read",
        "Read an identity-fenced complete graph source bundle for offline use.",
    ),
    scope(
        "sources.write",
        "sources",
        "write",
        "Submit stable offline source intents and their durable receipts.",
    ),
    scope(
        "sources.rebuild",
        "sources",
        "write",
        "Rebuild disposable graph projections from durable sources.",
    ),
    scope("mcp.tools.read", "mcp", "read", "List local MCP tools."),
    scope("mcp.tools.call", "mcp", "write", "Call local MCP tools."),
];

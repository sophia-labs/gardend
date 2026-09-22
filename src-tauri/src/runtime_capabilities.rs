use crate::runtime_config::{LOCAL_GRAPH_ORIGIN, LOCAL_PROVIDER_ID, RUNTIME_PROFILE};
use serde::Serialize;

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Capability {
    key: &'static str,
    status: &'static str,
    description: &'static str,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct RuntimeCapabilities {
    runtime_profile: &'static str,
    graph_origin: &'static str,
    provider_id: &'static str,
    hosted_available: bool,
    capabilities: Vec<Capability>,
}

#[cfg_attr(feature = "desktop", tauri::command)]
pub(crate) fn get_capabilities() -> RuntimeCapabilities {
    RuntimeCapabilities {
        runtime_profile: RUNTIME_PROFILE,
        graph_origin: LOCAL_GRAPH_ORIGIN,
        provider_id: LOCAL_PROVIDER_ID,
        hosted_available: false,
        capabilities: vec![
            Capability {
                key: "graph.local.list",
                status: "available",
                description: "List graphs from the local profile manifest.",
            },
            Capability {
                key: "graph.local.create",
                status: "available",
                description: "Create a durable local graph record under the profile directory.",
            },
            Capability {
                key: "graph.local.persist",
                status: "available",
                description: "Persist graph metadata to local files across app restarts.",
            },
            Capability {
                key: "document.local.persist",
                status: "available",
                description: "Create, edit, and reopen local document records under each graph.",
            },
            Capability {
                key: "document.local.crdt",
                status: "available",
                description: "Persist Yjs update bytes for TipTap Y.XmlFragment('content').",
            },
            Capability {
                key: "workspace.local.crdt",
                status: "available",
                description: "Persist graph-scoped Yjs workspace state for documents, folders, artifacts, and UI.",
            },
            Capability {
                key: "document.local.tiptap_tree",
                status: "available",
                description:
                    "Persist TipTap JSON/XML, materialized DocumentTree snapshots, and block projections.",
            },
            Capability {
                key: "rdf.local.store",
                status: "available",
                description: "Each local graph has an Oxigraph-backed RDF dataset.",
            },
            Capability {
                key: "sparql.local.query",
                status: "available",
                description: "SELECT, ASK, CONSTRUCT, and DESCRIBE run through Oxigraph.",
            },
            Capability {
                key: "sparql.local.update",
                status: "available",
                description: "SPARQL UPDATE mutates the local Oxigraph dataset.",
            },
            Capability {
                key: "semantic.local.embeddings",
                status: "available",
                description:
                    "Generate local block embeddings with fastembed after explicit local model setup.",
            },
            Capability {
                key: "semantic.local.model-setup",
                status: "available",
                description:
                    "Prepare the local embedding model into the user's Hugging Face cache without bundling it in the app.",
            },
            Capability {
                key: "semantic.local.model-catalog",
                status: "available",
                description:
                    "Expose local embedding model metadata and readiness as a catalog instead of a single implicit model.",
            },
            Capability {
                key: "artifact.local.ingestion-catalog",
                status: "available",
                description:
                    "Expose local artifact ingestion approaches, fidelity, runtime, and setup status for future parser selection.",
            },
            Capability {
                key: "loopback.local.api",
                status: "available",
                description:
                    "Expose a token-protected local API and MCP endpoint on 127.0.0.1 while the app is running.",
            },
            Capability {
                key: "graph.hosted.connect",
                status: "not_implemented",
                description: "Hosted graph provider will be added behind the provider boundary.",
            },
        ],
    }
}

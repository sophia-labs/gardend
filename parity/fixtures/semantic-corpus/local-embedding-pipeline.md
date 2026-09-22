# Local Embedding Pipeline

The local semantic index is designed for desktop-first retrieval. It should be
usable without a cloud embedding service, but it must still look like a normal
graph search capability to the rest of the product. The index reads materialized
document blocks, normalizes text, embeds the block content with a local model,
and stores a graph-scoped index under the profile directory.

## Setup Flow

The app exposes a setup panel instead of bundling every model file in the binary.
That keeps the application smaller and makes model updates possible without a
new app release. The panel reports whether the model is prepared, where the
cache lives, whether the model is loaded in memory, and whether the active graph
has stale documents waiting to be reindexed.

## Index Refresh

- Refresh should be explicit for heavy graphs.
- A CRDT flush should happen before indexing so projected blocks are current.
- The index status should report block count, document count, stale count, model ID, dimensions, and timestamp.
- Search should return document IDs, document titles, block IDs, block types, scores, order, and content snippets.

The first local provider uses fastembed because it is lightweight and Rust
friendly. Later providers can add Core ML, llama.cpp embeddings, hosted
OpenAI-compatible embedding endpoints, or BYOK cloud services behind the same
semantic provider interface. The API surface should not force callers to know
which embedding engine produced the vector.

## Privacy Constraints

Local-only mode should never send document text to a hosted embedding service by
default. If a user opts into BYOK or hosted embeddings, the app needs a clear
provider label, an explicit scope, and a way to rebuild the index when the
provider changes. Query logs should be local by default and should not leak
document content through diagnostics.

## Search Prompts

The phrase `graph scoped semantic index` should return this document. So should
`fastembed provider`, `stale documents waiting to be reindexed`, and `BYOK cloud
services behind the same provider interface`. These terms are deliberately
distinct from the PDF and CRDT fixtures so ranking differences are easier to
spot during parity tests.

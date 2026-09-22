# PDF Ingestion Playbook

PDF ingestion should start simple and preserve a path to higher fidelity. The
minimum useful flow is to accept a local PDF file, extract text page by page,
create a read-only source document, persist original file metadata, and index
the extracted blocks. A better flow also stores page anchors, text offsets,
detected headings, citation metadata, and a link back to the original binary.

## Lightweight Parser Options

- `pdf-extract` or Poppler-backed tools can provide fast plain-text extraction.
- `lopdf` can inspect structure and metadata without a large runtime dependency.
- `pdfium-render` can support page rendering and OCR integration later.
- A worker-style ingestion queue can be simulated locally with in-process jobs.

The ingestion surface should not assume that every document is editable prose.
A PDF-derived document may be read-only, may expose source-file metadata, and
may prefer page-aware block IDs such as `page-003-block-012`. Semantic search
should return the extracted block and enough source context to open the reader
near the relevant page.

## Structured Output

For parity work, every ingested PDF should produce a source file record with
filename, MIME type, byte size, storage key, and file type. The document record
should expose a title, read-only flag, body text, blocks, and source metadata in
the workspace snapshot. RDF should describe both the document and the source
artifact without confusing the original binary with editable content.

## Open Questions

- Should OCR be a separate optional provider because it pulls in larger models?
- Should page images be cached as artifacts or rendered on demand?
- Should citation extraction live in the parser or as a second enrichment pass?
- How much metadata should the local MCP server expose to agent clients?

Search for `page-aware block IDs`, `read-only source document`, `Poppler-backed`,
or `source artifact` to validate that semantic retrieval can distinguish this
PDF ingestion fixture from ordinary notes.

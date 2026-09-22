#!/usr/bin/env python3
"""Convert a PDF to Markdown using PyMuPDF4LLM for local native ingestion.

Emits per-page progress lines on stdout as `MN_PROGRESS {json}` so the
Rust process wrapper (`process_utils.rs::parse_progress_line`) can update
the local job registry's progress field. Final stdout line is the
single JSON result blob the caller deserializes.
"""

from __future__ import annotations

import argparse
import contextlib
import importlib.metadata
import json
import sys
import time
from pathlib import Path


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser()
    parser.add_argument("--input", required=True, help="PDF file path")
    parser.add_argument("--filename", required=True, help="Original filename")
    parser.add_argument("--title", help="Optional title override")
    return parser.parse_args()


def metadata_title(pdf_path: Path) -> tuple[str | None, int]:
    import fitz

    with fitz.open(pdf_path) as document:
        title = (document.metadata or {}).get("title") or None
        page_count = document.page_count
    title = title.strip() if title else None
    return title or None, page_count


def emit_progress(current: int, total: int, message: str) -> None:
    """Write an MN_PROGRESS line directly to the original stdout.

    Bypasses any active `contextlib.redirect_stdout(sys.stderr)` so progress
    markers reach the parent process's stdout for the Rust progress parser.
    """
    payload = {"current": current, "total": total, "phase": "parse", "message": message}
    sys.__stdout__.write(f"MN_PROGRESS {json.dumps(payload)}\n")
    sys.__stdout__.flush()


def main() -> int:
    args = parse_args()
    pdf_path = Path(args.input)
    if not pdf_path.is_file():
        raise SystemExit(f"input PDF does not exist: {pdf_path}")

    # pymupdf4llm may print dependency hints during import. Keep stdout as JSON-only.
    with contextlib.redirect_stdout(sys.stderr):
        import fitz
        import pymupdf4llm

    started = time.perf_counter()
    title, page_count = metadata_title(pdf_path)

    # Initial marker so consumers know how many pages to expect; covers the
    # zero-page case where the loop body never runs.
    emit_progress(0, page_count, f"page 0/{page_count}")

    markdown_parts: list[str] = []
    with contextlib.redirect_stdout(sys.stderr):
        for index in range(page_count):
            page_md = pymupdf4llm.to_markdown(
                str(pdf_path),
                pages=[index],
                show_progress=False,
                page_separators=False,
                ignore_images=True,
                ignore_graphics=False,
            )
            markdown_parts.append(page_md)
            emit_progress(index + 1, page_count, f"page {index + 1}/{page_count}")

    markdown = "".join(markdown_parts)
    elapsed_ms = (time.perf_counter() - started) * 1000
    title = (args.title or title or Path(args.filename).stem.replace("_", " ").replace("-", " ")).strip()
    non_empty_lines = [line for line in markdown.splitlines() if line.strip()]
    warnings: list[str] = []
    if not markdown.strip():
        warnings.append("PyMuPDF4LLM returned empty Markdown.")

    result = {
        "title": title or "Untitled PDF",
        "markdown": markdown,
        "warnings": warnings,
        "stats": {
            "parserName": "pymupdf4llm",
            "parser_name": "pymupdf4llm",
            "approachId": "pdf.pymupdf4llm",
            "approach_id": "pdf.pymupdf4llm",
            "pageCount": page_count,
            "page_count": page_count,
            "charCount": len(markdown),
            "char_count": len(markdown),
            "nonEmptyLineCount": len(non_empty_lines),
            "non_empty_line_count": len(non_empty_lines),
            "elapsedMs": round(elapsed_ms, 1),
            "elapsed_ms": round(elapsed_ms, 1),
            "pymupdf4llmVersion": importlib.metadata.version("pymupdf4llm"),
            "pymupdfVersion": fitz.version[0],
        },
        "pymupdf4llmVersion": importlib.metadata.version("pymupdf4llm"),
        "pymupdf_version": fitz.version[0],
    }
    print(json.dumps(result))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())

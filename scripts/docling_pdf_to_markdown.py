#!/usr/bin/env python3
"""Convert a PDF to Markdown with Docling for the local-native runtime."""

from __future__ import annotations

import argparse
import importlib.metadata
import io
import json
import sys
from pathlib import Path


def configure_rapidocr_cache(cache_dir: Path) -> None:
    cache_dir.mkdir(parents=True, exist_ok=True)
    try:
        from rapidocr.inference_engine.base import InferSession

        InferSession.DEFAULT_MODEL_PATH = cache_dir
    except Exception:
        pass

    try:
        from rapidocr.ch_ppocr_rec import main as rec_main

        rec_main.DEFAULT_MODEL_PATH = cache_dir
        rec_main.DEFAULT_DICT_PATH = cache_dir / "ppocr_keys_v1.txt"
    except Exception:
        pass


def extract_title(doc: object, filename: str) -> str:
    name = str(getattr(doc, "name", "") or "").strip()
    if name:
        return name

    try:
        from docling_core.types.doc import SectionHeaderItem

        for item, _level in doc.iterate_items():
            if isinstance(item, SectionHeaderItem):
                text = str(getattr(item, "text", "") or "").strip()
                if text:
                    return text
    except Exception:
        pass

    stem = Path(filename).stem
    return stem.replace("-", " ").replace("_", " ").strip().title() or "Untitled"


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--input", required=True)
    parser.add_argument("--filename", default="document.pdf")
    parser.add_argument("--title")
    parser.add_argument("--rapidocr-cache")
    args = parser.parse_args()

    from docling.datamodel.base_models import DocumentStream
    from docling.document_converter import DocumentConverter

    if args.rapidocr_cache:
        configure_rapidocr_cache(Path(args.rapidocr_cache))

    pdf_path = Path(args.input)
    content = pdf_path.read_bytes()
    converter = DocumentConverter()
    result = converter.convert(DocumentStream(name=args.filename, stream=io.BytesIO(content)))
    doc = result.document
    markdown = doc.export_to_markdown()
    page_count = len(getattr(doc, "pages", None) or [])
    warnings: list[str] = []
    if not markdown.strip():
        warnings.append("No content extracted; document may be scanned or image-based")
        markdown = "[No text extracted by Docling.]"

    print(json.dumps({
        "title": args.title or extract_title(doc, args.filename),
        "markdown": markdown,
        "warnings": warnings,
        "stats": {
            "pageCount": page_count,
            "page_count": page_count,
            "totalChars": len(markdown),
            "total_chars": len(markdown),
        },
        "parserName": "docling",
        "parser_name": "docling",
        "doclingVersion": importlib.metadata.version("docling"),
        "docling_version": importlib.metadata.version("docling"),
    }))
    return 0


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except Exception as exc:
        print(json.dumps({"error": str(exc)}), file=sys.stderr)
        raise

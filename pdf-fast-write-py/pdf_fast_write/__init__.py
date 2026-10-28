"""Sovereign PDF page subsetter — powered by Rust (pdf-fast-write)."""

from ._core import (
    extract_pdf_pages_from_bytes_py,
    extract_pdf_pages_py,
    extract_pdf_pages_to_bytes_py,
)


def extract_pages(source, pages, output):
    """Extract specific 1-based `pages` from `source` PDF into `output` PDF.

    Args:
        source: path to the source .pdf
        pages: list of 1-based page numbers to keep, in order
        output: path for the new subset .pdf
    """
    return extract_pdf_pages_py(source, list(pages), output)


def extract_pages_to_bytes(source, pages):
    """Extract specific 1-based `pages` from `source` PDF path; return
    bytes."""
    return extract_pdf_pages_to_bytes_py(source, list(pages))


def extract_pages_from_bytes(data, pages):
    """Extract specific 1-based `pages` from in-memory PDF `data`; return
    bytes."""
    return extract_pdf_pages_from_bytes_py(data, list(pages))


__all__ = [
    "extract_pages",
    "extract_pages_to_bytes",
    "extract_pages_from_bytes",
]

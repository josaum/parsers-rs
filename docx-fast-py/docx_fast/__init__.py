"""Read-only DOCX parsing from a path or in-memory bytes."""

import json

from ._core import (
    extract_docx_images_py,
    parse_docx_py,
    parse_docx_arrow_dataset_py,
    parse_docx_arrow_ipc_py,
    parse_docx_bytes_py,
    parse_docx_json_py,
    parse_docx_ndjson_py,
)


def parse_docx(path, max_inflate=None):
    """Parse a DOCX path without producing filesystem output."""
    return json.loads(parse_docx_py(path, max_inflate))


def parse_docx_bytes(data, max_inflate=None):
    """Parse an in-memory DOCX archive without filesystem output.

    A caller holding an upload should prefer this: the document never has to
    touch the filesystem to be read.
    """
    return json.loads(parse_docx_bytes_py(data, max_inflate))


def parse_docx_json(path, max_inflate=None):
    return json.loads(parse_docx_json_py(path, max_inflate))


def parse_docx_ndjson(path, out_path, max_inflate=None):
    return parse_docx_ndjson_py(path, out_path, max_inflate)


def parse_docx_arrow_ipc(path, out_path, max_inflate=None, batch_size=None):
    return parse_docx_arrow_ipc_py(path, out_path, max_inflate, batch_size)


def parse_docx_arrow_dataset(path, out_dir, max_inflate=None, batch_size=None):
    return parse_docx_arrow_dataset_py(path, out_dir, max_inflate, batch_size)


def extract_docx_images(path, out_dir):
    return extract_docx_images_py(path, out_dir)


__all__ = [
    "parse_docx",
    "parse_docx_bytes",
    "parse_docx_json",
    "parse_docx_ndjson",
    "parse_docx_arrow_ipc",
    "parse_docx_arrow_dataset",
    "extract_docx_images",
]

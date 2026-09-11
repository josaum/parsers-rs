"""Read-only PPTX parsing, from bytes or from a path.

`parse_pptx_bytes` is the in-memory entry point: it parses an upload where it
already is, with no temp file and no subprocess. The path-based functions parse
a presentation already on disk and either return its document JSON or stream it
out as NDJSON, Arrow IPC, or a directory of Arrow tables.
"""

import json

from ._core import (
    extract_pptx_images_py,
    parse_pptx_arrow_dataset_py,
    parse_pptx_arrow_ipc_bytes_py,
    parse_pptx_bytes_py,
    parse_pptx_json_py,
    parse_pptx_ndjson_py,
)


def parse_pptx(path, max_inflate=None):
    payload = parse_pptx_json_py(path, max_inflate)
    return json.loads(payload)


def parse_pptx_json(path, max_inflate=None):
    return parse_pptx_json_py(path, max_inflate)


def parse_pptx_ndjson(path, out_path, max_inflate=None):
    return parse_pptx_ndjson_py(path, out_path, max_inflate)


def parse_pptx_arrow_ipc_bytes(path, out_path, table, max_inflate=None, batch_size=None):
    """Encode one named shredded table to `out_path`; return the byte count."""
    return parse_pptx_arrow_ipc_bytes_py(
        path, out_path, table, max_inflate, batch_size
    )


def parse_pptx_arrow_dataset(path, out_dir, max_inflate=None, batch_size=None):
    """Write every shredded table into `out_dir`; return the paths written."""
    return parse_pptx_arrow_dataset_py(path, out_dir, max_inflate, batch_size)


def extract_pptx_images(path, out_dir, max_inflate=None):
    """Write every embedded image into `out_dir`; return the paths written."""
    return extract_pptx_images_py(path, out_dir, max_inflate)


def parse_pptx_bytes(data, max_inflate=None):
    """Parse a PPTX presentation from raw bytes (no temp file needed)."""
    payload = parse_pptx_bytes_py(data, max_inflate)
    return json.loads(payload)


__all__ = [
    "parse_pptx",
    "parse_pptx_bytes",
    "parse_pptx_json",
    "parse_pptx_ndjson",
    "parse_pptx_arrow_ipc_bytes",
    "parse_pptx_arrow_dataset",
    "extract_pptx_images",
]

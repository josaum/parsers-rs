"""Read-only XLSX parsing, from bytes or from a path.

`parse_xlsx_bytes` is the in-memory entry point: it parses an upload where it
already is, with no temp file and no subprocess. The path-based functions exist
for workbooks too large to want to materialize — `parse_xlsx_ndjson` and
`parse_xlsx_arrow_ipc` stream to a file instead of returning a workbook object.
"""

import json

from ._core import (
    parse_xlsx_arrow_ipc_py,
    parse_xlsx_bytes_py,
    parse_xlsx_json_py,
    parse_xlsx_ndjson_py,
)


def parse_xlsx(
    path, decode_dates=True, include_formulas=False, raw_values=False, max_inflate=None
):
    payload = parse_xlsx_json_py(
        path, decode_dates, include_formulas, raw_values, max_inflate
    )
    return json.loads(payload)


def parse_xlsx_json(
    path, decode_dates=True, include_formulas=False, raw_values=False, max_inflate=None
):
    return parse_xlsx_json_py(path, decode_dates, include_formulas, raw_values, max_inflate)


def parse_xlsx_ndjson(
    path,
    out_path,
    decode_dates=True,
    include_formulas=False,
    raw_values=False,
    max_inflate=None,
):
    return parse_xlsx_ndjson_py(
        path, out_path, decode_dates, include_formulas, raw_values, max_inflate
    )


def parse_xlsx_bytes(
    data, decode_dates=True, include_formulas=False, raw_values=False, max_inflate=None
):
    """Parse an XLSX workbook from raw bytes (no temp file needed)."""
    payload = parse_xlsx_bytes_py(
        data, decode_dates, include_formulas, raw_values, max_inflate
    )
    return json.loads(payload)


def parse_xlsx_arrow_ipc(
    path,
    out_path,
    decode_dates=True,
    include_formulas=False,
    raw_values=False,
    max_inflate=None,
    batch_size=None,
):
    """Stream cell batches to `out_path` as Arrow IPC; return the byte count."""
    return parse_xlsx_arrow_ipc_py(
        path,
        out_path,
        decode_dates,
        include_formulas,
        raw_values,
        max_inflate,
        batch_size,
    )


__all__ = [
    "parse_xlsx",
    "parse_xlsx_json",
    "parse_xlsx_bytes",
    "parse_xlsx_ndjson",
    "parse_xlsx_arrow_ipc",
]

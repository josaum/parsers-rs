"""Read-only XLS, XLSB and XLSX parsing, from bytes or from a path.

`parse_xlsx_bytes` is the in-memory entry point: it parses an upload where it
already is, with no temp file and no subprocess. The path-based functions exist
to write NDJSON or Arrow IPC without returning a Python workbook object.
XLSX emits cells incrementally; XLS/XLSB materialize a budgeted Rust workbook
before writing output.
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
                  path,
                  decode_dates=decode_dates,
                  include_formulas=include_formulas,
                  raw_values=raw_values,
                  max_inflate=max_inflate,
              )
    return json.loads(payload)


def parse_xlsx_json(
    path, decode_dates=True, include_formulas=False, raw_values=False, max_inflate=None
):
    return parse_xlsx_json_py(
               path,
               decode_dates=decode_dates,
               include_formulas=include_formulas,
               raw_values=raw_values,
               max_inflate=max_inflate,
           )


def parse_xlsx_ndjson(
    path,
    out_path,
    decode_dates=True,
    include_formulas=False,
    raw_values=False,
    max_inflate=None,
):
    return parse_xlsx_ndjson_py(
               path,
               out_path,
               decode_dates=decode_dates,
               include_formulas=include_formulas,
               raw_values=raw_values,
               max_inflate=max_inflate,
           )


def parse_xlsx_bytes(
    data, decode_dates=True, include_formulas=False, raw_values=False, max_inflate=None
):
    """Parse XLS, XLSB or XLSX bytes (no temp file needed)."""
    payload = parse_xlsx_bytes_py(
                  data,
                  decode_dates=decode_dates,
                  include_formulas=include_formulas,
                  raw_values=raw_values,
                  max_inflate=max_inflate,
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
               decode_dates=decode_dates,
               include_formulas=include_formulas,
               raw_values=raw_values,
               max_inflate=max_inflate,
               batch_size=batch_size,
           )


__all__ = [
    "parse_xlsx",
    "parse_xlsx_json",
    "parse_xlsx_bytes",
    "parse_xlsx_ndjson",
    "parse_xlsx_arrow_ipc",
    "parse_excel",
    "parse_excel_json",
    "parse_excel_bytes",
    "parse_excel_ndjson",
    "parse_excel_arrow_ipc",
]

# The historical names remain compatible; all readers detect XLS/XLSB/XLSX.
parse_excel = parse_xlsx
parse_excel_json = parse_xlsx_json
parse_excel_bytes = parse_xlsx_bytes
parse_excel_ndjson = parse_xlsx_ndjson
parse_excel_arrow_ipc = parse_xlsx_arrow_ipc

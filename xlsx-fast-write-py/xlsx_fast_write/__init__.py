"""Sovereign XLSX writer — powered by Rust (xlsx-fast-write)."""

import json as _json

from ._core import write_xlsx_json_py, write_xlsx_json_bytes_py


def write_xlsx(spec, out_path):
    """Write an XLSX file from a workbook spec (dict or JSON string).

    Spec shape::

        {
          "sheets": [
            {
              "name": "Summary",
              "column_widths": {"0": 12.0},
              "rows": [
                [{"t": "s", "v": "Header", "fmt": "header"}],
                [{"t": "n", "v": 42}]
              ]
            }
          ]
        }
    """
    payload = spec if isinstance(spec, str) else _json.dumps(spec)
    return write_xlsx_json_py(payload, out_path)


def write_xlsx_bytes(spec):
    """Write an XLSX from a workbook spec (dict or JSON string), return bytes."""
    payload = spec if isinstance(spec, str) else _json.dumps(spec)
    return write_xlsx_json_bytes_py(payload)


__all__ = ["write_xlsx", "write_xlsx_bytes"]

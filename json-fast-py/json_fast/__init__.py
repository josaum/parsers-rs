"""json-fast: SIMD JSON/JSONL parsing and JSON-family helpers powered by Rust.

Covers the six ECMA-404 JSON types (String, Number, Boolean, Null, Array,
Object) plus the derived formats: JSONL/NDJSON streaming, JSON Schema
validation, JSON-RPC 2.0 envelopes, JSON:API documents, JWT split/decode, and
JSON-LD context peek.
"""

from __future__ import annotations

from typing import Any

from json_fast._core import (
    JsonResourceError,
    count_lines,
    json_type,
    jsonapi_document,
    jsonld_meta,
    jsonrpc_request,
    jsonrpc_response,
    jwt_parts,
    loads,
    loads_many,
    validate,
    version,
)

__version__ = version()

__all__ = [
    "__version__",
    "JsonResourceError",
    "loads",
    "loads_many",
    "count_lines",
    "json_type",
    "validate",
    "jsonrpc_request",
    "jsonrpc_response",
    "jsonapi_document",
    "jwt_parts",
    "jsonld_meta",
]

"""
xsd-fast: High-performance XSD schema parser with Arrow derivation and XML validation.

Usage::

    from xsd_fast import list_elements, describe_element, validate, validate_bytes

    # List global elements in a schema
    elements = list_elements("/path/to/schema.xsd")

    # Get Arrow schema description as list of dicts
    fields = describe_element("/path/to/schema.xsd", "example")

    # Validate an XML file; returns list of {"severity","path","message"} dicts
    errors = validate("/path/to/schema.xsd", "/path/to/doc.xml")

    # Validate raw XML bytes
    errors = validate_bytes("/path/to/schema.xsd", xml_bytes)

    # Get Arrow IPC bytes for schema description
    from xsd_fast import schema_ipc_bytes
    ipc = schema_ipc_bytes("/path/to/schema.xsd", "example")
"""

from __future__ import annotations

import json

from ._core import (
    all_arrow_schemas_json_py,
    describe_element_bytes_json_py,
    describe_element_json_py,
    list_elements_bytes_py,
    list_elements_py,
    schema_ipc_bytes_py,
    validate_bytes_json_py,
    validate_json_py,
)


def list_elements(schema_path: str, max_includes: int = 64) -> list[str]:
    """Return all global element names defined in the schema."""
    return list_elements_py(
               schema_path,
               max_includes=max_includes,
           )


def list_elements_bytes(schema_bytes: bytes, max_includes: int = 64) -> list[str]:
    """Return global element names from in-memory XSD bytes."""
    return list_elements_bytes_py(
               schema_bytes,
               max_includes=max_includes,
           )


def describe_element(
    schema_path: str,
    element: str,
    max_depth: int = 8,
    map_dates: bool = True,
    include_attributes: bool = True,
) -> list[dict]:
    """Return Arrow schema description for a root element as a list of field dicts.

    Each dict has keys: ``path``, ``arrow_type``, ``nullable``.
    """
    payload = describe_element_json_py(
                  schema_path,
                  element,
                  max_depth=max_depth,
                  map_dates=map_dates,
                  include_attributes=include_attributes,
              )
    return json.loads(payload)


def describe_element_bytes(
    schema_bytes: bytes,
    element: str,
    max_depth: int = 8,
    map_dates: bool = True,
    include_attributes: bool = True,
) -> list[dict]:
    """Return an Arrow field description from in-memory XSD bytes."""
    payload = describe_element_bytes_json_py(
                  schema_bytes,
                  element,
                  max_depth=max_depth,
                  map_dates=map_dates,
                  include_attributes=include_attributes,
              )
    return json.loads(payload)


def validate(
    schema_path: str,
    xml_path: str,
    max_errors: int = 100,
    check_facets: bool = True,
    check_cardinality: bool = True,
) -> list[dict]:
    """Validate an XML file against the schema.

    Returns a list of ``{"severity": "Error"|"Warning", "path": "...", "message": "..."}`` dicts.
    An empty list means the document is valid.
    """
    payload = validate_json_py(
                  schema_path,
                  xml_path,
                  max_errors=max_errors,
                  check_facets=check_facets,
                  check_cardinality=check_cardinality,
              )
    return json.loads(payload)


def validate_bytes(
    schema_path: str,
    xml_bytes: bytes,
    max_errors: int = 100,
    check_facets: bool = True,
    check_cardinality: bool = True,
) -> list[dict]:
    """Validate an in-memory XML byte string against the schema.

    Returns a list of ``{"severity": "Error"|"Warning", "path": "...", "message": "..."}`` dicts.
    """
    payload = validate_bytes_json_py(
        schema_path,
        xml_bytes,
        max_errors=max_errors,
        check_facets=check_facets,
        check_cardinality=check_cardinality,
    )
    return json.loads(payload)


def schema_ipc_bytes(
    schema_path: str,
    element: str,
    max_depth: int = 8,
    map_dates: bool = True,
) -> bytes:
    """Return Arrow IPC schema description bytes for a root element."""
    return schema_ipc_bytes_py(
               schema_path,
               element,
               max_depth=max_depth,
               map_dates=map_dates,
           )



def all_arrow_schemas(
    schema_path: str,
    max_depth: int = 8,
    map_dates: bool = True,
    include_attributes: bool = True,
    max_includes: int = 64,
) -> dict[str, list[dict]]:
    """Return the Arrow field description of every global element, by name.

    Each value has the shape `describe_element` returns. Raises
    `RuntimeError` when any element's schema cannot be derived, rather than
    leaving that element out.
    """
    payload = all_arrow_schemas_json_py(
                  schema_path,
                  max_depth=max_depth,
                  map_dates=map_dates,
                  include_attributes=include_attributes,
                  max_includes=max_includes,
              )
    return json.loads(payload)


__all__ = [
    "all_arrow_schemas",
    "describe_element",
    "describe_element_bytes",
    "list_elements",
    "list_elements_bytes",
    "schema_ipc_bytes",
    "validate",
    "validate_bytes",
]

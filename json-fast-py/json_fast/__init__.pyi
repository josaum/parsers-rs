"""Type stubs for `json_fast`; checked against the module by the binding tests."""

from typing import Any, Dict, List, Literal, Optional

__version__: str

class JsonResourceError(RuntimeError):
    """Raised when configured JSON or JSONL resource limits are exceeded."""

def version() -> str: ...
def loads(
    s: str,
    *,
    max_input_bytes: Optional[int] = ...,
    max_nesting_depth: Optional[int] = ...,
    max_tokens: Optional[int] = ...,
    max_retained_bytes: Optional[int] = ...,
) -> Any: ...
def loads_many(
    jsonl_text: str,
    *,
    max_input_bytes: Optional[int] = ...,
    max_nesting_depth: Optional[int] = ...,
    max_tokens: Optional[int] = ...,
    max_lines: Optional[int] = ...,
    max_records: Optional[int] = ...,
    max_retained_bytes: Optional[int] = ...,
) -> List[Any]: ...
def count_lines(jsonl_text: str) -> int: ...
def json_type(s: str) -> Literal["string", "number", "boolean", "null", "array", "object"]: ...
def validate(
    s: str, schema: str, *, resources: Optional[Dict[str, str]] = ...
) -> List[Dict[str, str]]:
    """Validate `s` against `schema`; `resources` maps URI -> schema JSON text.

    Nothing is fetched. Documents are taken in URI order: when two claim one
    URI through `$id`, the first by URI order wins. A document that does not
    parse raises RuntimeError naming its URI.
    """
def jsonrpc_request(s: str) -> Dict[str, Any]: ...
def jsonrpc_response(s: str) -> Dict[str, Any]: ...
def jsonapi_document(s: str) -> Dict[str, Any]: ...
def jwt_parts(token: str) -> Dict[str, Any]: ...
def jsonld_meta(s: str) -> Dict[str, Any]: ...

__all__: List[str]

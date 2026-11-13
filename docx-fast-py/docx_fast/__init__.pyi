"""Type stubs for `docx_fast`; checked against the module by the binding tests."""

import os
from typing import Any, Dict, List, Optional, Union

def parse_docx(
    path: str,
    max_inflate: Optional[int] = ...,
) -> Dict[str, Any]: ...
def parse_docx_bytes(
    data: bytes,
    max_inflate: Optional[int] = ...,
) -> Dict[str, Any]: ...
def parse_docx_json(
    path: str,
    max_inflate: Optional[int] = ...,
) -> Dict[str, Any]: ...
def parse_docx_ndjson(
    path: str,
    out_path: str,
    max_inflate: Optional[int] = ...,
) -> None: ...
def parse_docx_arrow_ipc(
    path: str,
    out_path: str,
    max_inflate: Optional[int] = ...,
    batch_size: Optional[int] = ...,
) -> None: ...
def parse_docx_arrow_dataset(
    path: str,
    out_dir: str,
    max_inflate: Optional[int] = ...,
    batch_size: Optional[int] = ...,
) -> List[str]: ...
def extract_docx_images(
    path: str,
    out_dir: str,
) -> List[str]: ...

__all__: List[str]

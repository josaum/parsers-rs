"""Type stubs for `pptx_fast`; checked against the module by the binding tests."""

import os
from typing import Any, Dict, List, Optional, Union

def parse_pptx(
    path: Union[str, bytes, "os.PathLike[str]"],
    max_inflate: Optional[int] = ...,
) -> Dict[str, Any]: ...
def parse_pptx_json(
    path: Union[str, bytes, "os.PathLike[str]"],
    max_inflate: Optional[int] = ...,
) -> str: ...
def parse_pptx_ndjson(
    path: Union[str, bytes, "os.PathLike[str]"],
    out_path: Union[str, bytes, "os.PathLike[str]"],
    max_inflate: Optional[int] = ...,
) -> None: ...
def parse_pptx_arrow_ipc_bytes(
    path: Union[str, bytes, "os.PathLike[str]"],
    out_path: Union[str, bytes, "os.PathLike[str]"],
    table: str,
    max_inflate: Optional[int] = ...,
    batch_size: Optional[int] = ...,
) -> int: ...
def parse_pptx_arrow_dataset(
    path: Union[str, bytes, "os.PathLike[str]"],
    out_dir: Union[str, bytes, "os.PathLike[str]"],
    max_inflate: Optional[int] = ...,
    batch_size: Optional[int] = ...,
) -> List[str]: ...
def extract_pptx_images(
    path: Union[str, bytes, "os.PathLike[str]"],
    out_dir: Union[str, bytes, "os.PathLike[str]"],
    max_inflate: Optional[int] = ...,
) -> List[str]: ...
def parse_pptx_bytes(
    data: bytes,
    max_inflate: Optional[int] = ...,
) -> Dict[str, Any]: ...

__all__: List[str]

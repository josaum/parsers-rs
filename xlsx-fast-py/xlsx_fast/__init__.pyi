"""Type stubs for `xlsx_fast`; checked against the module by the binding tests."""

import os
from typing import Any, Dict, List, Optional, Union

def parse_xlsx(
    path: Union[str, bytes, "os.PathLike[str]"],
    decode_dates: bool = ...,
    include_formulas: bool = ...,
    raw_values: bool = ...,
    max_inflate: Optional[int] = ...,
) -> Dict[str, Any]: ...
def parse_xlsx_json(
    path: Union[str, bytes, "os.PathLike[str]"],
    decode_dates: bool = ...,
    include_formulas: bool = ...,
    raw_values: bool = ...,
    max_inflate: Optional[int] = ...,
) -> str: ...
def parse_xlsx_ndjson(
    path: Union[str, bytes, "os.PathLike[str]"],
    out_path: Union[str, bytes, "os.PathLike[str]"],
    decode_dates: bool = ...,
    include_formulas: bool = ...,
    raw_values: bool = ...,
    max_inflate: Optional[int] = ...,
) -> None: ...
def parse_xlsx_bytes(
    data: bytes,
    decode_dates: bool = ...,
    include_formulas: bool = ...,
    raw_values: bool = ...,
    max_inflate: Optional[int] = ...,
) -> Dict[str, Any]: ...
def parse_xlsx_arrow_ipc(
    path: Union[str, bytes, "os.PathLike[str]"],
    out_path: Union[str, bytes, "os.PathLike[str]"],
    decode_dates: bool = ...,
    include_formulas: bool = ...,
    raw_values: bool = ...,
    max_inflate: Optional[int] = ...,
    batch_size: Optional[int] = ...,
) -> int: ...

__all__: List[str]

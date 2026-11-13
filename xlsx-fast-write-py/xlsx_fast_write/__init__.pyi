"""Type stubs for `xlsx_fast_write`; checked against the module by the binding tests."""

import os
from typing import Any, Dict, List, Optional, Union

def write_xlsx(
    spec: Union[str, Dict[str, Any]],
    out_path: str,
) -> None: ...
def write_xlsx_bytes(
    spec: Union[str, Dict[str, Any]],
) -> bytes: ...

__all__: List[str]

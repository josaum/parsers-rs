"""Type stubs for `pdf_fast_write`; checked against the module by the binding tests."""

import os
from typing import Any, Dict, List, Optional, Union

def extract_pages(
    source: str,
    pages: List[int],
    output: str,
) -> None: ...
def extract_pages_to_bytes(
    source: str,
    pages: List[int],
) -> bytes: ...
def extract_pages_from_bytes(
    data: bytes,
    pages: List[int],
) -> bytes: ...

__all__: List[str]

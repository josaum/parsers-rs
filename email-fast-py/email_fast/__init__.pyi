"""Type stubs for `email_fast`; checked against the module by the binding tests."""

import os
from typing import Any, Dict, List, Optional, Union

def parse_email(
    path: str,
    max_attachment_size: Optional[int] = ...,
    include_raw_headers: bool = ...,
    html_to_text: bool = ...,
    extract_attachment_content: bool = ...,
    max_total_attachment_bytes: Optional[int] = ...,
) -> Dict[str, Any]: ...
def parse_eml(
    path: str,
    max_attachment_size: Optional[int] = ...,
    include_raw_headers: bool = ...,
    html_to_text: bool = ...,
    extract_attachment_content: bool = ...,
    max_total_attachment_bytes: Optional[int] = ...,
) -> Dict[str, Any]: ...
def parse_email_json(
    path: str,
    max_attachment_size: Optional[int] = ...,
    include_raw_headers: bool = ...,
    html_to_text: bool = ...,
    extract_attachment_content: bool = ...,
    max_total_attachment_bytes: Optional[int] = ...,
) -> str: ...
def parse_email_ndjson(
    path: str,
    out_path: str,
    max_attachment_size: Optional[int] = ...,
    include_raw_headers: bool = ...,
    html_to_text: bool = ...,
    extract_attachment_content: bool = ...,
    max_total_attachment_bytes: Optional[int] = ...,
) -> None: ...
def parse_mbox_ndjson(
    path: str,
    out_path: str,
    max_attachment_size: Optional[int] = ...,
    include_raw_headers: bool = ...,
    html_to_text: bool = ...,
    extract_attachment_content: bool = ...,
    max_total_attachment_bytes: Optional[int] = ...,
) -> int: ...
def parse_mbox_arrow(
    path: str,
    out_path: str,
    max_attachment_size: Optional[int] = ...,
    include_raw_headers: bool = ...,
    html_to_text: bool = ...,
    extract_attachment_content: bool = ...,
    max_total_attachment_bytes: Optional[int] = ...,
    batch_size: Optional[int] = ...,
) -> int: ...
def extract_attachments(
    path: str,
    out_dir: str,
    max_attachment_size: Optional[int] = ...,
    max_total_attachment_bytes: Optional[int] = ...,
) -> List[str]: ...
def parse_email_bytes(
    source: bytes,
    *,
    format: str,
    max_attachment_size: Optional[int] = ...,
    include_raw_headers: bool = ...,
    html_to_text: bool = ...,
    extract_attachment_content: bool = ...,
    max_total_attachment_bytes: Optional[int] = ...,
) -> Dict[str, Any]: ...
def parse_mbox_bytes(
    source: bytes,
    *,
    max_attachment_size: Optional[int] = ...,
    include_raw_headers: bool = ...,
    html_to_text: bool = ...,
    extract_attachment_content: bool = ...,
    max_total_attachment_bytes: Optional[int] = ...,
) -> List[Dict[str, Any]]: ...

__all__: List[str]

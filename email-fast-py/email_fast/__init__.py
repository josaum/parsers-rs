"""
email-fast - High-performance email parser powered by Rust

Supports .eml and .msg formats with attachment extraction and Arrow IPC output.
"""

import json

from ._core import (
    parse_email_bytes_json_py,
    parse_email_json_py,
    parse_eml_json_py,
    parse_email_ndjson_py,
    parse_mbox_ndjson_py,
    parse_mbox_arrow_py,
    extract_attachments_py,
)


def parse_email(
    path,
    max_attachment_size=None,
    include_raw_headers=False,
    html_to_text=False,
    extract_attachment_content=False,
    max_total_attachment_bytes=None,
):
    """
    Parse an email file (.eml or .msg) and return a Python dict.
    
    Args:
        path: Path to the email file
        max_attachment_size: Maximum attachment size to process (bytes)
        include_raw_headers: Include all raw headers in output
        html_to_text: Convert HTML body to plain text
        extract_attachment_content: Include attachment content (base64 encoded)
    
    Returns:
        Email dict with headers, body, attachments, etc.
    """
    payload = parse_email_json_py(
        path,
        max_attachment_size,
        include_raw_headers,
        html_to_text,
        extract_attachment_content,
        max_total_attachment_bytes,
    )
    return json.loads(payload)


def parse_eml(
    path,
    max_attachment_size=None,
    include_raw_headers=False,
    html_to_text=False,
    extract_attachment_content=False,
    max_total_attachment_bytes=None,
):
    """
    Parse an EML file and return a Python dict.
    
    Args:
        path: Path to the .eml file
        max_attachment_size: Maximum attachment size to process (bytes)
        include_raw_headers: Include all raw headers in output
        html_to_text: Convert HTML body to plain text
        extract_attachment_content: Include attachment content (base64 encoded)
    
    Returns:
        Email dict with headers, body, attachments, etc.
    """
    payload = parse_eml_json_py(
        path,
        max_attachment_size,
        include_raw_headers,
        html_to_text,
        extract_attachment_content,
        max_total_attachment_bytes,
    )
    return json.loads(payload)


def parse_email_json(
    path,
    max_attachment_size=None,
    include_raw_headers=False,
    html_to_text=False,
    extract_attachment_content=False,
    max_total_attachment_bytes=None,
):
    """
    Parse an email file and return raw JSON string.
    
    Args:
        path: Path to the email file
        max_attachment_size: Maximum attachment size to process (bytes)
        include_raw_headers: Include all raw headers in output
        html_to_text: Convert HTML body to plain text
        extract_attachment_content: Include attachment content (base64 encoded)
    
    Returns:
        JSON string
    """
    return parse_email_json_py(
        path,
        max_attachment_size,
        include_raw_headers,
        html_to_text,
        extract_attachment_content,
        max_total_attachment_bytes,
    )


def parse_email_ndjson(
    path,
    out_path,
    max_attachment_size=None,
    include_raw_headers=False,
    html_to_text=False,
    extract_attachment_content=False,
    max_total_attachment_bytes=None,
):
    """
    Parse an email file and write to NDJSON.
    
    Args:
        path: Path to the email file
        out_path: Path for the output .ndjson file
        max_attachment_size: Maximum attachment size to process (bytes)
        include_raw_headers: Include all raw headers in output
        html_to_text: Convert HTML body to plain text
        extract_attachment_content: Include attachment content
    """
    return parse_email_ndjson_py(
        path,
        out_path,
        max_attachment_size,
        include_raw_headers,
        html_to_text,
        extract_attachment_content,
        max_total_attachment_bytes,
    )


def parse_mbox_ndjson(
    path,
    out_path,
    max_attachment_size=None,
    include_raw_headers=False,
    html_to_text=False,
    extract_attachment_content=False,
    max_total_attachment_bytes=None,
):
    """
    Parse an mbox file and write to NDJSON.
    
    Args:
        path: Path to the .mbox file
        out_path: Path for the output .ndjson file
        max_attachment_size: Maximum attachment size to process (bytes)
        include_raw_headers: Include all raw headers in output
        html_to_text: Convert HTML body to plain text
        extract_attachment_content: Include attachment content
    
    Returns:
        Number of emails processed
    """
    return parse_mbox_ndjson_py(
        path,
        out_path,
        max_attachment_size,
        include_raw_headers,
        html_to_text,
        extract_attachment_content,
        max_total_attachment_bytes,
    )


def parse_mbox_arrow(
    path,
    out_path,
    max_attachment_size=None,
    include_raw_headers=False,
    html_to_text=False,
    extract_attachment_content=False,
    max_total_attachment_bytes=None,
    batch_size=None,
):
    """
    Parse an mbox file and write to Apache Arrow IPC format.
    
    Args:
        path: Path to the .mbox file
        out_path: Path for the output .arrow file(s)
        max_attachment_size: Maximum attachment size to process (bytes)
        include_raw_headers: Include all raw headers in output
        html_to_text: Convert HTML body to plain text
        extract_attachment_content: Include attachment content
        batch_size: Number of emails per batch (default: 10000)
    
    Returns:
        Number of emails processed
    """
    return parse_mbox_arrow_py(
        path,
        out_path,
        max_attachment_size,
        include_raw_headers,
        html_to_text,
        extract_attachment_content,
        max_total_attachment_bytes,
        batch_size,
    )


def extract_attachments(
    path, out_dir, max_attachment_size=None, max_total_attachment_bytes=None
):
    """
    Extract attachments from an email file.
    
    Args:
        path: Path to the email file
        out_dir: Output directory for extracted attachments
        max_attachment_size: Maximum attachment size to process (bytes)
    
    Returns:
        List of paths to extracted files
    """
    return extract_attachments_py(path, out_dir, max_attachment_size, max_total_attachment_bytes)



def parse_email_bytes(
    source,
    *,
    format,
    max_attachment_size=None,
    include_raw_headers=False,
    html_to_text=False,
    extract_attachment_content=False,
    max_total_attachment_bytes=None,
):
    """Parse EML or MSG bytes without creating or accepting a file path.

    A caller holding an uploaded document must not have to write it to disk
    first. Format is explicit because there is no filename to infer it from.
    """
    return json.loads(
        parse_email_bytes_json_py(
            source,
            format,
            max_attachment_size,
            include_raw_headers,
            html_to_text,
            extract_attachment_content,
        )
    )

__all__ = [
    "parse_email_bytes",
    "parse_email",
    "parse_eml",
    "parse_email_json",
    "parse_email_ndjson",
    "parse_mbox_ndjson",
    "parse_mbox_arrow",
    "extract_attachments",
]

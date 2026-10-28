"""Every advertised entry point must be callable, not merely importable.

This suite exists because `v2026.09.19` shipped a wheel in which **five of the
six** email entry points raised `TypeError` on any call:

    parse_eml_json_py() missing 1 required positional argument:
    'max_total_attachment_bytes'

The cause is a shape worth naming, because nothing in the build caught it.
`max_total_attachment_bytes` was added to the native functions, and the Python
wrapper was not updated to pass it. A wrapper/native arity mismatch **compiles
perfectly** — the native module is correct, the wrapper is correct Python, and
the two only meet at call time. The Rust equivalent of this same change failed
to compile in a consumer and was caught immediately; the Python side shipped.

Two further traps this pins:

* A `#[pyfunction]` with `Option<T>` parameters and **no** `#[pyo3(signature =
  ...)]` makes every one of them a REQUIRED positional argument. Adding an
  optional parameter to such a function is therefore a breaking change to every
  existing caller, silently. All entry points here now carry explicit
  signatures with defaults.
* A name in `__all__` that the module does not define breaks `from mod import
  *` for everyone, exposing nothing — `pdf_fast.__all__` advertised
  `FastPdfDataset`, which this repository never defined.

An import-surface test cannot see any of this: `hasattr` is true for a function
that raises on every call. Only calling it works.
"""

from __future__ import annotations

import json
from pathlib import Path

import pytest

email_fast = pytest.importorskip("email_fast")

EML = b"""From: sender@example.com
To: recipient@example.com
Subject: entry point smoke

body text
"""


@pytest.fixture()
def mbox_path(tmp_path: Path) -> str:
    """A real mbox: the arrow writer emits nothing for a bare .eml."""
    path = tmp_path / "box.mbox"
    path.write_bytes(b"From sender@example.com Mon Sep 21 00:00:00 2026\n" + EML)
    return str(path)


@pytest.fixture()
def eml_path(tmp_path: Path) -> str:
    path = tmp_path / "message.eml"
    path.write_bytes(EML)
    return str(path)


def test_every_public_name_resolves() -> None:
    """`from email_fast import *` must not fail, and did for pdf_fast."""
    missing = [n for n in getattr(email_fast, "__all__", []) if not hasattr(email_fast, n)]
    assert missing == [], f"__all__ advertises names the module does not define: {missing}"


def test_parse_eml_is_callable_with_defaults(eml_path: str) -> None:
    parsed = email_fast.parse_eml(eml_path)
    assert parsed["subject"] == "entry point smoke"


def test_parse_email_is_callable_with_defaults(eml_path: str) -> None:
    parsed = email_fast.parse_email(eml_path)
    assert parsed["subject"] == "entry point smoke"


def test_parse_email_json_is_callable_with_defaults(eml_path: str) -> None:
    payload = email_fast.parse_email_json(eml_path)
    assert json.loads(payload)["subject"] == "entry point smoke"


def test_parse_email_bytes_is_callable_with_defaults() -> None:
    # `format` is keyword-only by design: there is no filename to infer it from.
    parsed = email_fast.parse_email_bytes(EML, format="eml")
    assert parsed["subject"] == "entry point smoke"


def test_parse_email_ndjson_is_callable_with_defaults(eml_path: str, tmp_path: Path) -> None:
    out = tmp_path / "out.ndjson"
    email_fast.parse_email_ndjson(eml_path, str(out))
    assert out.read_text(encoding="utf-8").strip(), "ndjson output must not be empty"


def test_parse_mbox_ndjson_is_callable_with_defaults(mbox_path: str, tmp_path: Path) -> None:
    out = tmp_path / "box.ndjson"
    email_fast.parse_mbox_ndjson(mbox_path, str(out))
    assert out.read_text(encoding="utf-8").strip(), "ndjson output must not be empty"


def test_parse_mbox_arrow_is_callable_with_defaults(mbox_path: str, tmp_path: Path) -> None:
    """Omitted from the first version of this suite, and broken on a DIFFERENT
    argument (`batch_size`) — which is why a guard must cover every advertised
    entry point, not a representative one."""
    out = tmp_path / "out.arrow"
    email_fast.parse_mbox_arrow(mbox_path, str(out))
    assert out.exists() or list(tmp_path.glob("out*")), "arrow output must be written"


def test_wrappers_forward_optionals_by_keyword() -> None:
    """Positional forwarding is how a silent misalignment becomes possible.

    `parse_mbox_arrow` passed `batch_size` into the slot the native function
    reads as `max_total_attachment_bytes`. That RAISED while the native
    signatures lacked defaults, which is the only reason anyone noticed. Adding
    defaults — the fix for the other seven functions — would have converted
    that loud failure into a silent one, feeding a batch size to an attachment
    budget forever. Keyword forwarding makes the whole class impossible rather
    than merely tested, so this asserts the style, not one call site.
    """
    import re

    source = (Path(__file__).resolve().parents[1] / "email_fast" / "__init__.py").read_text(encoding="utf-8")
    optionals = (
        "max_attachment_size",
        "include_raw_headers",
        "html_to_text",
        "extract_attachment_content",
        "max_total_attachment_bytes",
        "batch_size",
    )
    positional = []
    for call in re.finditer(r"\w+_py\(\n(.*?)\n\s*\)", source, re.DOTALL):
        for line in call.group(1).splitlines():
            bare = line.strip().rstrip(",")
            if bare in optionals:
                positional.append(bare)
    assert positional == [], (
        "these optionals are forwarded positionally, so reordering the native "
        f"signature would silently misalign them: {sorted(set(positional))}"
    )


def test_every_entry_point_has_a_test(eml_path: str) -> None:
    """The suite must not silently stop covering a function someone adds."""
    import inspect

    public = {
        name
        for name in dir(email_fast)
        if not name.startswith("_") and inspect.isfunction(getattr(email_fast, name))
    }
    tested = {
        name
        for name in public
        if any(name in fn for fn in globals() if fn.startswith("test_"))
    }
    assert public - tested == set(), f"entry points with no test: {sorted(public - tested)}"


def test_extract_attachments_is_callable_with_defaults(eml_path: str, tmp_path: Path) -> None:
    # No attachments in the fixture: the contract under test is that the call
    # succeeds with its documented defaults, not that it finds anything.
    assert email_fast.extract_attachments(eml_path, str(tmp_path / "att")) == []


def test_the_aggregate_budget_is_reachable_from_python(eml_path: str) -> None:
    """The parameter whose addition broke the wheel must now be passable.

    Fixing the arity by dropping the parameter would make the tests above pass
    while leaving the capability unreachable, so assert it explicitly.
    """
    parsed = email_fast.parse_eml(eml_path, max_total_attachment_bytes=1024)
    assert parsed["subject"] == "entry point smoke"

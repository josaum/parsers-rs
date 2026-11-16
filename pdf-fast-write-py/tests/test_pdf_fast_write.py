"""Every `pdf_fast_write` entry point: a round trip and its errors.

Every input is built here with the standard library, so the suite needs no
fixture file and no network.
"""

from __future__ import annotations

import ast
import re
import inspect
import sys
import threading
from pathlib import Path

import pytest

MODULE = pytest.importorskip("pdf_fast_write")


def times_another_thread_ran_during(call, attempts: int = 5) -> int:
    """How many of `attempts` calls let a second Python thread run meanwhile.

    The switch interval is raised so the interpreter never hands the GIL over
    between bytecodes of the calling thread. A second thread, made runnable
    just before `call`, can then run only if `call` releases the GIL itself:
    with the GIL held the count is exactly 0, whatever the machine's load.
    No duration is compared with a threshold.
    """
    previous = sys.getswitchinterval()
    sys.setswitchinterval(1000.0)
    count = 0
    try:
        for _ in range(attempts):
            go = threading.Event()
            ran: list = []

            def waiter() -> None:
                go.wait()
                ran.append(True)

            thread = threading.Thread(target=waiter)
            thread.start()  # returns once the waiter is blocked in go.wait()
            go.set()
            call()
            count += bool(ran)
            thread.join()
    finally:
        sys.setswitchinterval(previous)
    return count


def holds_the_gil() -> int:
    """A call that never releases the GIL: the control for the test below."""
    return sum(range(3_000_000))


def pdf_bytes(pages: int = 3) -> bytes:
    """A classic-xref PDF with `pages` empty pages, offsets computed here."""
    kids = " ".join(f"{3 + i} 0 R" for i in range(pages))
    objects = [
        b"<< /Type /Catalog /Pages 2 0 R >>",
        f"<< /Type /Pages /Kids [{kids}] /Count {pages} >>".encode(),
    ]
    for index in range(pages):
        width = 200 + index
        objects.append(
            f"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 {width} 300] >>".encode()
        )
    out = bytearray(b"%PDF-1.4\n")
    offsets = []
    for number, body in enumerate(objects, start=1):
        offsets.append(len(out))
        out += f"{number} 0 obj\n".encode() + body + b"\nendobj\n"
    xref = len(out)
    out += f"xref\n0 {len(objects) + 1}\n".encode()
    out += b"0000000000 65535 f \n"
    for offset in offsets:
        out += f"{offset:010d} 00000 n \n".encode()
    out += f"trailer\n<< /Size {len(objects) + 1} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n".encode()
    return bytes(out)


def page_count(data: bytes) -> int:
    assert data.startswith(b"%PDF-")
    return len(re.findall(rb"/Type\s*/Page(?![a-zA-Z])", data))


@pytest.fixture()
def pdf_path(tmp_path: Path) -> str:
    path = tmp_path / "source.pdf"
    path.write_bytes(pdf_bytes())
    return str(path)


def test_extract_pages_from_bytes_keeps_only_the_chosen_pages() -> None:
    subset = MODULE.extract_pages_from_bytes(pdf_bytes(), [1, 3])
    assert isinstance(subset, bytes)
    assert page_count(subset) == 2
    # Pages are told apart by their widths: 200, 201, 202.
    assert b"201" not in subset


def test_extract_pages_to_bytes_and_to_a_path_agree(pdf_path: str, tmp_path: Path) -> None:
    subset = MODULE.extract_pages_to_bytes(pdf_path, [2])
    assert page_count(subset) == 1
    out = tmp_path / "subset.pdf"
    assert MODULE.extract_pages(pdf_path, [2], str(out)) is None
    assert page_count(out.read_bytes()) == 1


def test_a_page_outside_the_document_is_a_runtime_error(pdf_path: str, tmp_path: Path) -> None:
    with pytest.raises(RuntimeError):
        MODULE.extract_pages_from_bytes(pdf_bytes(), [99])
    with pytest.raises(RuntimeError):
        MODULE.extract_pages_to_bytes(pdf_path, [99])
    with pytest.raises(RuntimeError):
        MODULE.extract_pages(pdf_path, [99], str(tmp_path / "o.pdf"))


def test_missing_and_malformed_sources_are_runtime_errors(tmp_path: Path) -> None:
    with pytest.raises(RuntimeError):
        MODULE.extract_pages_to_bytes(str(tmp_path / "missing.pdf"), [1])
    with pytest.raises(RuntimeError):
        MODULE.extract_pages(str(tmp_path / "missing.pdf"), [1], str(tmp_path / "o.pdf"))
    with pytest.raises(RuntimeError):
        MODULE.extract_pages_from_bytes(b"not a pdf", [1])


def test_wrong_argument_types_are_refused() -> None:
    with pytest.raises(TypeError):
        MODULE.extract_pages_from_bytes("text", [1])
    with pytest.raises((TypeError, OverflowError)):
        MODULE.extract_pages_from_bytes(pdf_bytes(), [-1])


def test_extracting_releases_the_gil() -> None:
    data = pdf_bytes(600)
    pages = list(range(1, 601))
    assert times_another_thread_ran_during(holds_the_gil) == 0
    assert times_another_thread_ran_during(lambda: MODULE.extract_pages_from_bytes(data, pages)) >= 1


def test_the_wheel_ships_type_information_that_matches_the_module() -> None:
    package_dir = Path(MODULE.__file__).parent
    assert (package_dir / "py.typed").is_file()
    public = {
        name
        for name in dir(MODULE)
        if not name.startswith("_") and inspect.isfunction(getattr(MODULE, name))
    }
    assert public == {name for name in MODULE.__all__ if not name.startswith("_")}
    stub = package_dir / "__init__.pyi"
    if not stub.is_file():
        # No stub: the wrappers carry their own annotations.
        for name in public:
            assert "return" in getattr(MODULE, name).__annotations__, name
        return
    declared = {
        node.name: [a.arg for a in node.args.args + node.args.kwonlyargs]
        for node in ast.parse(stub.read_text(encoding="utf-8")).body
        if isinstance(node, ast.FunctionDef)
    }
    assert set(declared) == public
    for name, params in declared.items():
        assert params == list(inspect.signature(getattr(MODULE, name)).parameters), name

"""Every `pdf_fast_write` entry point: a round trip and its errors.

Every input is built here with the standard library, so the suite needs no
fixture file and no network.
"""

from __future__ import annotations

import ast
import re
import inspect
import threading
from pathlib import Path

import pytest

MODULE = pytest.importorskip("pdf_fast_write")


def in_two_threads(call):
    """Run `call` on two threads at once and return both results."""
    results: list = [None, None]

    def work(slot: int) -> None:
        results[slot] = call()

    threads = [threading.Thread(target=work, args=(slot,)) for slot in (0, 1)]
    for thread in threads:
        thread.start()
    for thread in threads:
        thread.join(timeout=60)
    assert not any(thread.is_alive() for thread in threads)
    return results


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


def test_two_threads_extract_at_once() -> None:
    data = pdf_bytes(40)
    first, second = in_two_threads(lambda: MODULE.extract_pages_from_bytes(data, [1, 2]))
    assert page_count(first) == page_count(second) == 2


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

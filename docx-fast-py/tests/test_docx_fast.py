"""Every `docx_fast` entry point: a round trip, its defaults and its errors.

Every input is built here with the standard library, so the suite needs no
fixture file and no network.
"""

from __future__ import annotations

import ast
import io
import json
import zipfile
import inspect
import threading
from pathlib import Path

import pytest

MODULE = pytest.importorskip("docx_fast")


RELS_NS = "http://schemas.openxmlformats.org/package/2006/relationships"
OFFICE_NS = "http://schemas.openxmlformats.org/officeDocument/2006/relationships"


def rels(*entries: tuple) -> str:
    body = "".join(
        f'<Relationship Id="{rid}" Type="{OFFICE_NS}/{kind}" Target="{target}"/>'
        for rid, kind, target in entries
    )
    return f'<?xml version="1.0" encoding="UTF-8"?>\n<Relationships xmlns="{RELS_NS}">{body}</Relationships>'


def archive(parts: dict) -> bytes:
    buffer = io.BytesIO()
    with zipfile.ZipFile(buffer, "w", zipfile.ZIP_DEFLATED) as zf:
        for name, text in parts.items():
            zf.writestr(name, text)
    return buffer.getvalue()


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


def docx_bytes(text: str = "Hello DOCX") -> bytes:
    return archive(
        {
            "_rels/.rels": rels(("rId1", "officeDocument", "word/document.xml")),
            "word/_rels/document.xml.rels": rels(),
            "word/document.xml": (
                '<?xml version="1.0" encoding="UTF-8"?>'
                '<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:body>'
                f"<w:p><w:r><w:t>{text}</w:t></w:r></w:p>"
                "<w:tbl><w:tr><w:tc><w:p><w:r><w:t>Cell 1</w:t></w:r></w:p></w:tc></w:tr></w:tbl>"
                "</w:body></w:document>"
            ),
        }
    )


@pytest.fixture()
def docx_path(tmp_path: Path) -> str:
    path = tmp_path / "doc.docx"
    path.write_bytes(docx_bytes())
    return str(path)


def test_parse_docx_bytes_round_trips_the_text() -> None:
    document = MODULE.parse_docx_bytes(docx_bytes())
    assert "Hello DOCX" in json.dumps(document["blocks"])
    assert "Cell 1" in json.dumps(document["blocks"])


def test_parse_docx_and_parse_docx_json_agree_with_the_bytes_form(docx_path: str) -> None:
    expected = MODULE.parse_docx_bytes(docx_bytes())
    assert MODULE.parse_docx(docx_path)["blocks"] == expected["blocks"]
    assert MODULE.parse_docx_json(docx_path, max_inflate=1 << 20)["blocks"] == expected["blocks"]


def test_parse_docx_ndjson_writes_one_json_record_per_line(docx_path: str, tmp_path: Path) -> None:
    out = tmp_path / "out.ndjson"
    assert MODULE.parse_docx_ndjson(docx_path, str(out)) is None
    lines = out.read_text(encoding="utf-8").splitlines()
    assert lines and all(isinstance(json.loads(line), dict) for line in lines)
    assert "Hello DOCX" in out.read_text(encoding="utf-8")


def test_parse_docx_arrow_outputs_are_readable_by_pyarrow(docx_path: str, tmp_path: Path) -> None:
    ipc = pytest.importorskip("pyarrow.ipc")
    ipc_dir = tmp_path / "ipc"
    MODULE.parse_docx_arrow_ipc(docx_path, str(ipc_dir), batch_size=1)
    written = MODULE.parse_docx_arrow_dataset(docx_path, str(tmp_path / "dataset"))
    assert written and all(Path(p).is_file() for p in written)
    by_name = {}
    for table_path in written:
        with open(table_path, "rb") as handle:
            try:
                table = ipc.open_file(handle).read_all()
            except Exception:
                handle.seek(0)
                table = ipc.open_stream(handle).read_all()
        by_name[Path(table_path).name] = table
        assert table.schema.names, table_path
    assert sum(table.num_rows for table in by_name.values()) > 0
    assert sorted(p.name for p in ipc_dir.iterdir()) == sorted(by_name)


def test_extract_docx_images_returns_no_paths_for_a_document_without_images(
    docx_path: str, tmp_path: Path
) -> None:
    assert MODULE.extract_docx_images(docx_path, str(tmp_path / "img")) == []


def test_errors_are_runtime_errors_for_bad_documents(tmp_path: Path) -> None:
    with pytest.raises(RuntimeError):
        MODULE.parse_docx(str(tmp_path / "missing.docx"))
    with pytest.raises(RuntimeError):
        MODULE.parse_docx_bytes(b"not a zip archive")
    with pytest.raises(RuntimeError):
        MODULE.parse_docx_json(str(tmp_path / "missing.docx"))
    with pytest.raises(RuntimeError):
        MODULE.parse_docx_ndjson(str(tmp_path / "missing.docx"), str(tmp_path / "o"))
    with pytest.raises(RuntimeError):
        MODULE.parse_docx_arrow_ipc(str(tmp_path / "missing.docx"), str(tmp_path / "o"))
    with pytest.raises(RuntimeError):
        MODULE.parse_docx_arrow_dataset(str(tmp_path / "missing.docx"), str(tmp_path / "o"))
    with pytest.raises(RuntimeError):
        MODULE.extract_docx_images(str(tmp_path / "missing.docx"), str(tmp_path / "o"))


def test_the_inflate_budget_is_enforced() -> None:
    big = docx_bytes("x" * 200_000)
    assert MODULE.parse_docx_bytes(big)["blocks"]
    with pytest.raises(RuntimeError):
        MODULE.parse_docx_bytes(big, max_inflate=1024)


@pytest.mark.parametrize("bad", [None, 7, "text"])
def test_wrong_argument_types_raise_type_error(bad: object) -> None:
    with pytest.raises(TypeError):
        MODULE.parse_docx_bytes(bad)


def test_two_threads_parse_at_once() -> None:
    data = docx_bytes("threaded " * 5_000)
    first, second = in_two_threads(lambda: MODULE.parse_docx_bytes(data))
    assert first == second and "threaded" in json.dumps(first["blocks"])


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

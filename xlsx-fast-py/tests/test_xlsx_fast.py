"""Every `xlsx_fast` entry point: a round trip, its defaults and its errors.

Every input is built here with the standard library, so the suite needs no
fixture file and no network.
"""

from __future__ import annotations

import ast
import io
import json
import zipfile
import inspect
import sys
import threading
from pathlib import Path

import pytest

MODULE = pytest.importorskip("xlsx_fast")


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


SHEET_NS = "http://schemas.openxmlformats.org/spreadsheetml/2006/main"


def xlsx_bytes(text: str = "Hello XLSX") -> bytes:
    return archive(
        {
            "_rels/.rels": rels(("rId1", "officeDocument", "xl/workbook.xml")),
            "xl/workbook.xml": (
                '<?xml version="1.0" encoding="UTF-8"?>'
                f'<workbook xmlns="{SHEET_NS}" xmlns:r="{OFFICE_NS}"><sheets>'
                '<sheet name="Sheet1" sheetId="1" r:id="rId1"/></sheets></workbook>'
            ),
            "xl/_rels/workbook.xml.rels": rels(
                ("rId1", "worksheet", "worksheets/sheet1.xml"),
                ("rId2", "sharedStrings", "sharedStrings.xml"),
            ),
            "xl/sharedStrings.xml": (
                '<?xml version="1.0" encoding="UTF-8"?>'
                f'<sst xmlns="{SHEET_NS}" count="1" uniqueCount="1"><si><t>{text}</t></si></sst>'
            ),
            "xl/worksheets/sheet1.xml": (
                '<?xml version="1.0" encoding="UTF-8"?>'
                f'<worksheet xmlns="{SHEET_NS}"><sheetData><row r="1">'
                '<c r="A1" t="s"><v>0</v></c><c r="B1"><v>42</v></c>'
                '<c r="C1"><f>B1*2</f><v>84</v></c>'
                "</row></sheetData></worksheet>"
            ),
        }
    )


@pytest.fixture()
def xlsx_path(tmp_path: Path) -> Path:
    path = tmp_path / "book.xlsx"
    path.write_bytes(xlsx_bytes())
    return path


def test_parse_xlsx_bytes_round_trips_the_cells() -> None:
    workbook = MODULE.parse_xlsx_bytes(xlsx_bytes())
    cells = json.dumps(workbook["sheets"][0]["cells"])
    assert "Hello XLSX" in cells and "42" in cells


def test_path_forms_accept_str_and_pathlike_and_agree(xlsx_path: Path) -> None:
    expected = MODULE.parse_xlsx_bytes(xlsx_bytes())
    assert MODULE.parse_xlsx(xlsx_path) == expected
    assert MODULE.parse_xlsx(str(xlsx_path)) == expected
    assert json.loads(MODULE.parse_xlsx_json(xlsx_path)) == expected


def test_include_formulas_is_off_by_default_and_forwarded_by_keyword() -> None:
    plain = json.dumps(MODULE.parse_xlsx_bytes(xlsx_bytes()))
    with_formulas = json.dumps(MODULE.parse_xlsx_bytes(xlsx_bytes(), include_formulas=True))
    assert "B1*2" not in plain
    assert "B1*2" in with_formulas


def test_parse_xlsx_ndjson_writes_one_json_record_per_line(xlsx_path: Path, tmp_path: Path) -> None:
    out = tmp_path / "out.ndjson"
    assert MODULE.parse_xlsx_ndjson(xlsx_path, out) is None
    lines = out.read_text(encoding="utf-8").splitlines()
    assert lines and all(isinstance(json.loads(line), dict) for line in lines)
    assert "Hello XLSX" in out.read_text(encoding="utf-8")


def test_parse_xlsx_arrow_ipc_is_readable_by_pyarrow(xlsx_path: Path, tmp_path: Path) -> None:
    ipc = pytest.importorskip("pyarrow.ipc")
    out = tmp_path / "cells.arrow"
    count = MODULE.parse_xlsx_arrow_ipc(xlsx_path, out, batch_size=1)
    assert isinstance(count, int) and count > 0
    with open(out, "rb") as handle:
        try:
            table = ipc.open_file(handle).read_all()
        except Exception:
            handle.seek(0)
            table = ipc.open_stream(handle).read_all()
    assert table.num_rows == 3
    assert table.schema.names
    assert "Hello XLSX" in json.dumps(table.to_pylist(), default=str)


def test_errors_are_runtime_errors_for_bad_workbooks(tmp_path: Path) -> None:
    missing = tmp_path / "missing.xlsx"
    with pytest.raises(RuntimeError):
        MODULE.parse_xlsx(missing)
    with pytest.raises(RuntimeError):
        MODULE.parse_xlsx_json(missing)
    with pytest.raises(RuntimeError):
        MODULE.parse_xlsx_ndjson(missing, tmp_path / "o")
    with pytest.raises(RuntimeError):
        MODULE.parse_xlsx_arrow_ipc(missing, tmp_path / "o")
    with pytest.raises(RuntimeError):
        MODULE.parse_xlsx_bytes(b"not a zip archive")
    with pytest.raises(RuntimeError, match="expected a str or os.PathLike"):
        MODULE.parse_xlsx(7)


def test_the_inflate_budget_is_enforced() -> None:
    big = xlsx_bytes("x" * 200_000)
    assert MODULE.parse_xlsx_bytes(big)["sheets"]
    with pytest.raises(RuntimeError):
        MODULE.parse_xlsx_bytes(big, max_inflate=1024)


@pytest.mark.parametrize("bad", [None, 7, "text"])
def test_wrong_argument_types_raise_type_error(bad: object) -> None:
    with pytest.raises(TypeError):
        MODULE.parse_xlsx_bytes(bad)


def test_parsing_releases_the_gil() -> None:
    data = xlsx_bytes("threaded " * 200_000)
    assert times_another_thread_ran_during(holds_the_gil) == 0
    assert times_another_thread_ran_during(lambda: MODULE.parse_xlsx_bytes(data)) >= 1


SNAPSHOT_TYPES = {
    "Utf8": "string", "LargeUtf8": "large_string", "Boolean": "bool", "Binary": "binary",
    "UInt8": "uint8", "UInt16": "uint16", "UInt32": "uint32", "UInt64": "uint64",
    "Int8": "int8", "Int16": "int16", "Int32": "int32", "Int64": "int64",
    "Float32": "float", "Float64": "double", "Date32": "date32[day]",
    "Timestamp(Microsecond, None)": "timestamp[us]",
    "Timestamp(Millisecond, None)": "timestamp[ms]",
    "List(Field { data_type: Int32, nullable: true })": "list<item: int32>",
}


def assert_matches_schema_snapshot(schema, core_crate: str, snapshot: str) -> None:
    """The schema pyarrow read is the one the core crate's snapshot freezes."""
    import re

    path = Path(__file__).resolve().parents[2] / core_crate / "tests" / "schema_snapshots" / snapshot
    if not path.is_file():
        pytest.skip(f"no schema snapshot at {path} (tests run outside the repository)")
    expected = []
    for line in path.read_text(encoding="utf-8").splitlines():
        match = re.fullmatch(r"(.+?): (.+)\(nullable=(true|false)\)", line.strip())
        if match:
            expected.append((match.group(1), match.group(2), match.group(3) == "true"))
    assert expected, f"{path} holds no fields"
    assert schema.names == [name for name, _, _ in expected]
    for field, (name, rust_type, nullable) in zip(schema, expected):
        assert rust_type in SNAPSHOT_TYPES, f"{name}: no pyarrow spelling known for {rust_type}"
        assert str(field.type) == SNAPSHOT_TYPES[rust_type], name
        assert field.nullable is nullable, name


def test_parse_xlsx_arrow_ipc_matches_the_core_schema_snapshot(xlsx_path: Path, tmp_path: Path) -> None:
    ipc = pytest.importorskip("pyarrow.ipc")
    out = tmp_path / "cells.arrow"
    MODULE.parse_xlsx_arrow_ipc(xlsx_path, out)
    with open(out, "rb") as handle:
        try:
            schema = ipc.open_file(handle).schema
        except Exception:
            handle.seek(0)
            schema = ipc.open_stream(handle).schema
    assert_matches_schema_snapshot(schema, "xlsx-fast-core", "schema.snap")


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

"""Every `xlsx_fast_write` entry point: a round trip and its errors.

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

MODULE = pytest.importorskip("xlsx_fast_write")


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


SPEC = {
    "sheets": [
        {
            "name": "Summary",
            "column_widths": {"0": 12.0},
            "rows": [
                [{"t": "s", "v": "Header", "fmt": "header"}, {"t": "s", "v": "Value"}],
                [{"t": "s", "v": "Alpha"}, {"t": "n", "v": 42}],
            ],
        }
    ]
}


def workbook_xml(data: bytes) -> str:
    with zipfile.ZipFile(io.BytesIO(data)) as zf:
        assert zf.testzip() is None
        assert "xl/workbook.xml" in zf.namelist()
        return "".join(
            zf.read(name).decode("utf-8") for name in zf.namelist() if name.endswith(".xml")
        )


def test_write_xlsx_bytes_accepts_a_dict_and_a_json_string() -> None:
    from_dict = MODULE.write_xlsx_bytes(SPEC)
    from_text = MODULE.write_xlsx_bytes(json.dumps(SPEC))
    assert isinstance(from_dict, bytes)
    for data in (from_dict, from_text):
        xml = workbook_xml(data)
        assert "Summary" in xml and "Alpha" in xml and "42" in xml


def test_write_xlsx_writes_the_same_workbook_to_a_path(tmp_path: Path) -> None:
    out = tmp_path / "out.xlsx"
    assert MODULE.write_xlsx(SPEC, str(out)) is None
    assert "Alpha" in workbook_xml(out.read_bytes())


def test_a_malformed_spec_is_a_runtime_error(tmp_path: Path) -> None:
    with pytest.raises(RuntimeError):
        MODULE.write_xlsx_bytes("{not json")
    with pytest.raises(RuntimeError):
        MODULE.write_xlsx("{not json", str(tmp_path / "out.xlsx"))


def test_an_unwritable_path_is_a_runtime_error(tmp_path: Path) -> None:
    with pytest.raises(RuntimeError):
        MODULE.write_xlsx(SPEC, str(tmp_path / "no-such-dir" / "out.xlsx"))


def test_a_wrong_path_type_is_a_type_error() -> None:
    with pytest.raises(TypeError):
        MODULE.write_xlsx(SPEC, 7)


def test_writing_releases_the_gil() -> None:
    row = [{"t": "s", "v": "cell"}, {"t": "n", "v": 42}] * 10
    spec = json.dumps({"sheets": [{"name": "Big", "rows": [row] * 20_000}]})
    assert times_another_thread_ran_during(holds_the_gil) == 0
    assert times_another_thread_ran_during(lambda: MODULE.write_xlsx_bytes(spec)) >= 1


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

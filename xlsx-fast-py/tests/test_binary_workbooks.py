from pathlib import Path

import pytest
import xlsx_fast

FIXTURES = Path(__file__).resolve().parents[2] / "xlsx-fast-core" / "tests" / "fixtures"


@pytest.mark.parametrize("name", ["date.xls", "date.xlsb", "date_1904.xls", "date_1904.xlsb"])
def test_binary_path_bytes_dates_and_ndjson(name, tmp_path):
    source = FIXTURES / name
    workbook = xlsx_fast.parse_excel(source)
    assert xlsx_fast.parse_excel_bytes(source.read_bytes()) == workbook
    assert xlsx_fast.parse_xlsx(source) == workbook
    assert workbook["sheets"][0]["cells"][0]["value"]["iso"] == "2021-01-01T00:00:00"
    output = tmp_path / "cells.ndjson"
    xlsx_fast.parse_excel_ndjson(source, output)
    assert len(output.read_text().splitlines()) == 7
    ipc = tmp_path / "cells.arrow"
    written = xlsx_fast.parse_excel_arrow_ipc(source, ipc, batch_size=2)
    assert written == ipc.stat().st_size
    import pyarrow.ipc
    with ipc.open("rb") as stream:
        table = pyarrow.ipc.open_stream(stream).read_all()
    assert table.num_rows == 6
    assert table["date_iso"][0].as_py() == "2021-01-01T00:00:00"


@pytest.mark.parametrize("name", ["xls_formula.xls", "formula_values.xlsb"])
def test_binary_formulas(name):
    workbook = xlsx_fast.parse_excel(FIXTURES / name, include_formulas=True)
    assert any(cell.get("formula") for sheet in workbook["sheets"] for cell in sheet["cells"])


@pytest.mark.parametrize("name", ["password_protected.xls", "password_protected.xlsb", "too_small.xls"])
def test_binary_errors(name):
    with pytest.raises(RuntimeError):
        xlsx_fast.parse_excel(FIXTURES / name)


@pytest.mark.parametrize("name", ["date.xls", "date.xlsb"])
def test_binary_budget(name):
    with pytest.raises(RuntimeError, match="max_inflate"):
        xlsx_fast.parse_excel(FIXTURES / name, max_inflate=16)

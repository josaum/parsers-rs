"""Every `pdf_fast` entry point: a round trip, its errors, and a refused render.

Each PDF is built here with the standard library, so the suite needs no
fixture file and no network.
"""

from __future__ import annotations

import ast
import inspect
import json
import threading
from pathlib import Path

import pytest

MODULE = pytest.importorskip("pdf_fast")


def pdf(objects: dict) -> bytes:
    """A classic-xref PDF from `{object number: body}`; object 1 is the root."""
    out = bytearray(b"%PDF-1.7\n")
    top = max(objects)
    offsets = {}
    for number, body in sorted(objects.items()):
        offsets[number] = len(out)
        out += f"{number} 0 obj\n".encode() + body + b"\nendobj\n"
    xref = len(out)
    out += f"xref\n0 {top + 1}\n".encode()
    for number in range(top + 1):
        if number in offsets:
            out += f"{offsets[number]:010d} 00000 n \n".encode()
        else:
            out += b"0000000000 65535 f \n"
    out += f"trailer\n<< /Size {top + 1} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n".encode()
    return bytes(out)


def stream(dictionary: str, body: str) -> bytes:
    return f"<< {dictionary} /Length {len(body)} >>\nstream\n{body}\nendstream".encode()


def text_pdf(first: str = "Hello PDF", second: str = "Second page") -> bytes:
    """Two 300 x 200 pages of Helvetica text."""

    def page(contents: int) -> bytes:
        return (
            f"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 300 200] /Contents {contents} 0 R "
            "/Resources << /Font << /F1 7 0 R >> >> >>"
        ).encode()

    return pdf(
        {
            1: b"<< /Type /Catalog /Pages 2 0 R >>",
            2: b"<< /Type /Pages /Kids [3 0 R 5 0 R] /Count 2 >>",
            3: page(4),
            4: stream("", f"BT /F1 24 Tf 20 100 Td ({first}) Tj ET"),
            5: page(6),
            6: stream("", f"BT /F1 24 Tf 20 100 Td ({second}) Tj ET"),
            7: b"<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>",
        }
    )


def self_drawing_form_pdf() -> bytes:
    """A page that draws a form which draws itself: endless recursion if followed.

    Case 2 of `pdf-fast-core/tests/render_guard.rs`: the form has no
    resources and paints nothing but itself.
    """
    return pdf(
        {
            1: b"<< /Type /Catalog /Pages 2 0 R >>",
            2: b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>",
            3: (
                b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 100 100] /Contents 4 0 R "
                b"/Resources << /XObject << /F1 10 0 R >> >> >>"
            ),
            4: stream("", "/F1 Do"),
            10: stream("/Type /XObject /Subtype /Form /BBox [0 0 100 100]", "/F1 Do"),
        }
    )


@pytest.fixture()
def pdf_path(tmp_path: Path) -> str:
    path = tmp_path / "text.pdf"
    path.write_bytes(text_pdf())
    return str(path)


@pytest.fixture()
def bad_path(tmp_path: Path) -> str:
    path = tmp_path / "bad.pdf"
    path.write_bytes(b"not a pdf")
    return str(path)


def read_ipc(payload: bytes):
    ipc = pytest.importorskip("pyarrow.ipc")
    try:
        return ipc.open_stream(payload).read_all()
    except Exception:
        return ipc.open_file(payload).read_all()


def with_reading_order(call):
    """Call with `reading_order=True`; a build without the optional
    `reading-order` feature must refuse with the documented `ValueError`."""
    try:
        return call()
    except ValueError as error:
        assert "reading-order" in str(error)
        return None


OCR_RUNS = [{"text": "cell", "px_x": 10.0, "px_y": 20.0, "px_height": 12.0}]


# -- text and structure ------------------------------------------------------


def test_parse_pdf_text_reads_both_pages_from_a_path_and_from_bytes(pdf_path: str) -> None:
    text = MODULE.parse_pdf_text(pdf_path)
    assert "Hello PDF" in text and "Second page" in text
    assert text.index("Hello PDF") < text.index("Second page")
    assert MODULE.parse_pdf_text_bytes(text_pdf()) == text
    ordered = with_reading_order(lambda: MODULE.parse_pdf_text(pdf_path, reading_order=True))
    assert ordered is None or "Hello PDF" in ordered


def test_parse_pdf_text_returns_diagnostics_on_request(pdf_path: str) -> None:
    text, diagnostics = MODULE.parse_pdf_text(pdf_path, return_diagnostics=True)
    assert isinstance(diagnostics, MODULE.FullTextDiagnostics)
    assert diagnostics.pages == 2 and diagnostics.chars >= len("Hello PDF")
    assert "FullTextDiagnostics" in repr(diagnostics)
    text_b, diagnostics_b = MODULE.parse_pdf_text_bytes(text_pdf(), return_diagnostics=True)
    assert text_b == text and diagnostics_b.pages == 2


def test_parse_pdf_and_its_json_forms_agree(pdf_path: str) -> None:
    document = MODULE.parse_pdf(pdf_path)
    assert isinstance(document, dict)
    assert json.loads(MODULE.parse_pdf_json(pdf_path)) == document
    assert "Hello PDF" in MODULE.parse_pdf_semantic_runs_json(pdf_path)
    paragraphs = MODULE.parse_pdf_paragraphs(pdf_path)
    assert [p["page_index"] for p in paragraphs] == [0, 1]
    assert "Hello PDF" in paragraphs[0]["text"]


def test_page_count_and_dimensions(pdf_path: str) -> None:
    assert MODULE.parse_pdf_page_count(pdf_path) == 2
    dimensions = MODULE.parse_pdf_page_dimensions(pdf_path)
    assert [d.page_index for d in dimensions] == [0, 1]
    assert (dimensions[0].width, dimensions[0].height, dimensions[0].rotate) == (300.0, 200.0, 0)
    assert isinstance(dimensions[0], MODULE.PageDimension)
    assert "PageDimension" in repr(dimensions[0])


# -- Arrow -------------------------------------------------------------------


def test_document_level_arrow_ipc_is_readable_by_pyarrow(pdf_path: str) -> None:
    runs = read_ipc(MODULE.parse_pdf_semantic_runs_arrow_ipc(pdf_path))
    assert runs.num_rows >= 2
    assert "Hello PDF" in json.dumps(runs.to_pylist(), default=str)
    pages = read_ipc(MODULE.parse_pdf_page_multimodal_arrow_ipc(pdf_path))
    assert pages.num_rows >= 1 and pages.schema.names


def test_page_level_arrow_ipc_is_readable_by_pyarrow(pdf_path: str) -> None:
    for call in (
        MODULE.parse_pdf_page_table_layout_arrow_ipc,
        MODULE.parse_pdf_page_table_arrow_ipc,
    ):
        assert read_ipc(call(pdf_path, 0)).schema.names
        assert read_ipc(call(pdf_path, 0, epsilon=1.0)).schema.names


def test_ocr_runs_record_batch_builds_the_intake_schema() -> None:
    pytest.importorskip("pyarrow")
    batch = MODULE.ocr_runs_record_batch(OCR_RUNS)
    assert batch.schema.names == ["text", "px_x", "px_y", "px_height"]
    assert [str(t) for t in batch.schema.types] == ["string", "float", "float", "float"]
    assert MODULE.ocr_runs_record_batch(batch) is batch
    with pytest.raises(ValueError, match="missing required key 'px_y'"):
        MODULE.ocr_runs_record_batch([{"text": "x", "px_x": 1.0, "px_height": 2.0}])


def test_from_ocr_arrow_ipc_accepts_a_record_batch(pdf_path: str) -> None:
    pytest.importorskip("pyarrow")
    batch = MODULE.ocr_runs_record_batch(OCR_RUNS)
    for call in (
        MODULE.parse_pdf_page_table_layout_from_ocr_arrow_ipc,
        MODULE.parse_pdf_page_table_from_ocr_arrow_ipc,
    ):
        assert read_ipc(call(pdf_path, 0, batch)).schema.names
        assert read_ipc(call(pdf_path, 0, batch, epsilon=1.0, dpi=150.0)).schema.names


def test_deprecated_c_data_pointers_import_or_release(pdf_path: str) -> None:
    pytest.importorskip("pyarrow")
    batch = MODULE.ocr_runs_record_batch(OCR_RUNS)
    calls = [
        lambda: MODULE.parse_pdf_page_multimodal_c_data(pdf_path, 0),
        lambda: MODULE.parse_pdf_page_table_layout_c_data(pdf_path, 0),
        lambda: MODULE.parse_pdf_page_table_c_data(pdf_path, 0, epsilon=1.0),
        lambda: MODULE.parse_pdf_page_table_layout_from_ocr_c_data(pdf_path, 0, batch),
        lambda: MODULE.parse_pdf_page_table_from_ocr_c_data(pdf_path, 0, batch, dpi=150.0),
    ]
    for call in calls:
        array_ptr, schema_ptr = call()
        imported = MODULE._import_record_batch_from_c(array_ptr, schema_ptr)
        assert imported.schema.names
        # Importing freed the pair, so a second release is refused.
        with pytest.raises(ValueError):
            MODULE.release_arrow_c_data(array_ptr, schema_ptr)
    # Pointers that are not imported are freed through the release helper.
    array_ptr, schema_ptr = calls[0]()
    assert MODULE.release_arrow_c_data(array_ptr, schema_ptr) is None
    with pytest.raises(ValueError):
        MODULE.release_arrow_c_data(array_ptr, schema_ptr)


def test_releasing_pointers_this_module_did_not_issue_frees_nothing() -> None:
    # Two made-up integers used to be passed to Box::from_raw.
    for array_ptr, schema_ptr in [(1, 2), (0x1000, 0x2000), (0, 0)]:
        with pytest.raises(ValueError):
            MODULE.release_arrow_c_data(array_ptr, schema_ptr)


def test_fast_pdf_page_scanner_reuses_one_open_document(pdf_path: str) -> None:
    pytest.importorskip("pyarrow")
    scanner = MODULE.FastPdfPageScanner(pdf_path)
    assert scanner.path == pdf_path
    assert scanner.page_count() == 2
    assert [d.page_index for d in scanner.page_dimensions()] == [0, 1]
    batch = MODULE.ocr_runs_record_batch(OCR_RUNS)
    assert scanner.get_page_batch(0).schema.names
    assert scanner.get_page_table_batch(0).schema.names
    assert scanner.get_page_tables_batch(0, epsilon=1.0).schema.names
    assert scanner.get_page_table_batch_from_ocr(0, batch).schema.names
    assert scanner.get_page_tables_batch_from_ocr(0, batch, dpi=150.0).schema.names
    assert len(list(scanner.stream_pages())) == 2
    assert len(list(scanner.stream_page_batches())) == 2


# -- the PdfScanner class ----------------------------------------------------


def test_pdf_scanner_methods_round_trip(pdf_path: str) -> None:
    scanner = MODULE.PdfScanner(pdf_path)
    assert len(scanner) == 2 and scanner.page_count == 2
    assert "PdfScanner" in repr(scanner)
    assert json.loads(scanner.scan_json()) == MODULE.parse_pdf(pdf_path)
    assert "Hello PDF" in scanner.semantic_runs_json()
    assert "Hello PDF" in scanner.paragraphs_json()
    assert read_ipc(scanner.semantic_runs_arrow_ipc()).num_rows >= 2
    assert read_ipc(scanner.page_multimodal_arrow_ipc()).schema.names
    assert read_ipc(scanner.page_table_layout_arrow_ipc(0)).schema.names
    assert read_ipc(scanner.page_table_arrow_ipc(0)).schema.names
    assert len(json.loads(scanner.page_dimensions_json())) == 2
    assert [d.page_index for d in scanner.page_dimensions()] == [0, 1]
    text, diagnostics = scanner.full_text()
    assert "Hello PDF" in text and diagnostics.pages == 2
    ordered = with_reading_order(lambda: scanner.full_text(reading_order=True))
    assert ordered is None or "Hello PDF" in ordered[0]
    blocks = with_reading_order(lambda: scanner.reading_order_blocks_json(0))
    assert blocks is None or isinstance(json.loads(blocks), (list, dict))
    debug = with_reading_order(lambda: scanner.debug_reading_order_blocks(0))
    assert debug is None or isinstance(debug, str)
    for method in (
        "page_text_line_boxes",
        "page_vector_glyph_origins",
    ):
        assert getattr(scanner, method)(0) is not None
    for method in (
        "vector_glyph_clusters",
        "vector_glyph_cluster_lines",
        "vector_glyph_descriptor_lines",
    ):
        assert getattr(scanner, method)() is not None


def test_pdf_scanner_capsules_import_into_pyarrow(pdf_path: str) -> None:
    pytest.importorskip("pyarrow")
    scanner = MODULE.PdfScanner(pdf_path)
    batch = MODULE.ocr_runs_record_batch(OCR_RUNS)
    for capsules in (
        scanner.page_multimodal_arrow_c_array(0),
        scanner.page_table_layout_arrow_c_array(0),
        scanner.page_table_arrow_c_array(0),
        scanner.page_table_layout_from_ocr_arrow_c_array(0, batch),
        scanner.page_table_from_ocr_arrow_c_array(0, batch),
    ):
        assert MODULE._import_record_batch_from_capsules(*capsules).schema.names
    assert read_ipc(scanner.page_table_layout_from_ocr_arrow_ipc(0, batch)).schema.names
    assert read_ipc(scanner.page_table_from_ocr_arrow_ipc(0, batch)).schema.names
    for pointers in (
        scanner.page_multimodal_c_data(0),
        scanner.page_table_layout_c_data(0),
        scanner.page_table_c_data(0),
        scanner.page_table_layout_from_ocr_c_data(0, batch),
        scanner.page_table_from_ocr_c_data(0, batch),
    ):
        assert MODULE._import_record_batch_from_c(*pointers).schema.names


# -- rendering ---------------------------------------------------------------


def test_rasterized_png_is_a_png_or_none(pdf_path: str) -> None:
    for png in (
        MODULE.parse_pdf_page_rasterized_png(pdf_path, 0),
        MODULE.parse_pdf_page_rasterized_png(pdf_path, 0, dpi=72.0),
        MODULE.PdfScanner(pdf_path).page_rasterized_png(0, 72.0),
    ):
        assert png is None or bytes(png).startswith(b"\x89PNG\r\n\x1a\n")
    interpreted = MODULE.PdfScanner(pdf_path).page_rasterized_png_interpreted(0, 72.0)
    assert interpreted is not None and bytes(interpreted).startswith(b"\x89PNG\r\n\x1a\n")


def test_bitmap_components_is_none_for_a_page_without_an_image(pdf_path: str) -> None:
    assert MODULE.parse_pdf_page_bitmap_components(pdf_path, 0) is None
    assert json.loads(MODULE.PdfScanner(pdf_path).page_bitmap_components_json(0)) is None


def test_a_form_that_draws_itself_is_refused_not_followed(tmp_path: Path) -> None:
    """The render must raise; following the form would overflow the stack and
    kill the interpreter."""
    path = tmp_path / "self.pdf"
    path.write_bytes(self_drawing_form_pdf())
    scanner = MODULE.PdfScanner(str(path))
    with pytest.raises(Exception, match="interpreted render refused"):
        scanner.page_rasterized_png_interpreted(0, 72.0)
    with pytest.raises(Exception):
        scanner.page_rasterized_png(0, 72.0)
    with pytest.raises(Exception):
        MODULE.parse_pdf_page_rasterized_png(str(path), 0, dpi=72.0)
    # The interpreter is still alive and the document still answers.
    assert scanner.page_count == 1


# -- errors ------------------------------------------------------------------


def test_a_missing_file_raises(tmp_path: Path) -> None:
    missing = str(tmp_path / "missing.pdf")
    for call in (
        MODULE.parse_pdf,
        MODULE.parse_pdf_json,
        MODULE.parse_pdf_semantic_runs_json,
        MODULE.parse_pdf_paragraphs,
        MODULE.parse_pdf_semantic_runs_arrow_ipc,
        MODULE.parse_pdf_page_multimodal_arrow_ipc,
        MODULE.parse_pdf_page_count,
        MODULE.parse_pdf_page_dimensions,
        MODULE.parse_pdf_text,
        MODULE.PdfScanner,
        MODULE.FastPdfPageScanner,
    ):
        with pytest.raises(Exception):
            call(missing)
    for call in (
        MODULE.parse_pdf_page_multimodal_c_data,
        MODULE.parse_pdf_page_table_layout_arrow_ipc,
        MODULE.parse_pdf_page_table_layout_c_data,
        MODULE.parse_pdf_page_table_arrow_ipc,
        MODULE.parse_pdf_page_table_c_data,
        MODULE.parse_pdf_page_bitmap_components,
        MODULE.parse_pdf_page_rasterized_png,
    ):
        with pytest.raises(Exception):
            call(missing, 0)


def test_bytes_that_are_not_a_pdf_read_as_an_empty_document(bad_path: str) -> None:
    """The parser is lenient: malformed input is an empty document, not an error."""
    assert MODULE.parse_pdf_page_count(bad_path) == 0
    assert MODULE.parse_pdf_text(bad_path) == ""
    assert MODULE.parse_pdf_text_bytes(b"not a pdf") == ""
    assert MODULE.parse_pdf_paragraphs(bad_path) == []
    assert MODULE.parse_pdf_page_dimensions(bad_path) == []
    assert len(MODULE.PdfScanner(bad_path)) == 0
    # Any page of an empty document is out of range.
    with pytest.raises(Exception):
        MODULE.parse_pdf_page_table_arrow_ipc(bad_path, 0)


def test_wrong_argument_types_raise_type_error() -> None:
    with pytest.raises(TypeError):
        MODULE.parse_pdf_text_bytes("text, not bytes")
    with pytest.raises(TypeError):
        MODULE.parse_pdf_text(7)


def test_a_page_out_of_range_raises(pdf_path: str) -> None:
    scanner = MODULE.PdfScanner(pdf_path)
    fast = MODULE.FastPdfPageScanner(pdf_path)
    for call in (
        lambda: MODULE.parse_pdf_page_table_layout_arrow_ipc(pdf_path, 99),
        lambda: MODULE.parse_pdf_page_table_arrow_ipc(pdf_path, 99),
        lambda: MODULE.parse_pdf_page_multimodal_c_data(pdf_path, 99),
        lambda: scanner.page_table_layout_arrow_ipc(99),
        lambda: scanner.page_multimodal_arrow_c_array(99),
        lambda: fast.get_page_batch(99),
        lambda: fast.get_page_table_batch(99),
    ):
        with pytest.raises(Exception):
            call()
    with pytest.raises((OverflowError, TypeError)):
        scanner.page_table_layout_arrow_ipc(-1)


def test_malformed_ocr_runs_raise(pdf_path: str) -> None:
    with pytest.raises(ValueError, match="missing required key 'px_x'"):
        MODULE.parse_pdf_page_table_from_ocr_arrow_ipc(pdf_path, 0, [{"text": "x"}])
    with pytest.raises(TypeError):
        MODULE.parse_pdf_page_table_layout_from_ocr_arrow_ipc(pdf_path, 0, object())
    with pytest.raises(RuntimeError):
        MODULE.parse_pdf_page_table_from_ocr_arrow_ipc(pdf_path, 0, "not json")


# -- threads and types -------------------------------------------------------


def test_two_threads_parse_at_once() -> None:
    data = text_pdf("threaded text", "second threaded page")
    results: list = [None, None]

    def work(slot: int) -> None:
        results[slot] = MODULE.parse_pdf_text_bytes(data)

    threads = [threading.Thread(target=work, args=(slot,)) for slot in (0, 1)]
    for thread in threads:
        thread.start()
    for thread in threads:
        thread.join(timeout=60)
    assert results[0] == results[1] and "threaded text" in results[0]


def test_from_pdf_fast_import_star_resolves_every_name() -> None:
    namespace: dict = {}
    exec("from pdf_fast import *", namespace)  # noqa: S102
    assert set(MODULE.__all__) <= set(namespace)


def test_the_wheel_ships_type_information_that_matches_the_module() -> None:
    package_dir = Path(MODULE.__file__).parent
    assert (package_dir / "py.typed").is_file()
    tree = ast.parse((package_dir / "__init__.pyi").read_text(encoding="utf-8"))
    functions = {
        node.name: [a.arg for a in node.args.args + node.args.kwonlyargs]
        for node in tree.body
        if isinstance(node, ast.FunctionDef)
    }
    classes = {
        node.name: {
            item.name: [a.arg for a in item.args.args]
            for item in node.body
            if isinstance(item, ast.FunctionDef)
        }
        for node in tree.body
        if isinstance(node, ast.ClassDef)
    }
    assert set(functions) | set(classes) == set(MODULE.__all__)
    for name, params in functions.items():
        assert params == list(inspect.signature(getattr(MODULE, name)).parameters), name
    for name, params in classes["FastPdfPageScanner"].items():
        real = inspect.signature(getattr(MODULE.FastPdfPageScanner, name)).parameters
        assert params == list(real), name
    public_methods = {
        name
        for name, value in vars(MODULE.FastPdfPageScanner).items()
        if inspect.isfunction(value) and not name.startswith("_")
    }
    assert public_methods == set(classes["FastPdfPageScanner"]) - {"__init__"}

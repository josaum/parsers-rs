import json

from ._core import (
    FullTextDiagnostics,
    PageDimension,
    PdfScanner,
    release_arrow_c_data_py,
    scan_pdf_full_text_bytes_py,
    scan_pdf_full_text_py,
    scan_pdf_json_py,
    scan_pdf_page_bitmap_components_json_py,
    scan_pdf_page_count_py,
    scan_pdf_page_multimodal_arrow_ipc_py,
    scan_pdf_page_multimodal_c_data_py,
    scan_pdf_page_rasterized_png_py,
    scan_pdf_page_table_arrow_ipc_py,
    scan_pdf_page_table_c_data_py,
    scan_pdf_page_table_from_ocr_arrow_ipc_py,
    scan_pdf_page_table_from_ocr_c_data_py,
    scan_pdf_page_table_layout_arrow_ipc_py,
    scan_pdf_page_table_layout_c_data_py,
    scan_pdf_page_table_layout_from_ocr_arrow_ipc_py,
    scan_pdf_page_table_layout_from_ocr_c_data_py,
    scan_pdf_paragraphs_json_py,
    scan_pdf_semantic_runs_arrow_ipc_py,
    scan_pdf_semantic_runs_json_py,
)


def parse_pdf(path):
    return json.loads(scan_pdf_json_py(path))


def parse_pdf_json(path):
    return scan_pdf_json_py(path)


def parse_pdf_semantic_runs_json(path):
    return scan_pdf_semantic_runs_json_py(path)


def parse_pdf_paragraphs(path):
    """Return the document's text runs coalesced into paragraph blocks.

    Calls `pdf_fast_core::paragraph::merge_paragraphs` directly — the
    same Rust primitive used by `parse_pdf_text(reading_order=True)`
    and (transitively) by the gateway OCR pipeline. Bench harnesses
    should prefer this over rolling their own merge so the bench's
    block universe stays identical to production's.

    Returns a list of dicts, one per paragraph, with fields:
      * `id` (paragraph id, dense 0..N-1 in page-then-y order)
      * `page_index`
      * `text` (lines joined with `\\n`)
      * `font_size`
      * `x`, `y`, `w`, `h` (union bbox)
      * `line_run_indices` (indices into `parse_pdf_semantic_runs_json`
        for label projection)

    Multi-page-safe: paragraphs never span pages.
    """
    return json.loads(scan_pdf_paragraphs_json_py(path))


def parse_pdf_semantic_runs_arrow_ipc(path):
    return scan_pdf_semantic_runs_arrow_ipc_py(path)


def parse_pdf_page_multimodal_arrow_ipc(path):
    return scan_pdf_page_multimodal_arrow_ipc_py(path)


def parse_pdf_page_multimodal_c_data(path, page_index):
    """Deprecated: prefer ``FastPdfPageScanner.get_page_batch`` (PyCapsule).

    Returns raw Arrow C Data pointers that must be imported or freed via
    ``release_arrow_c_data``.
    """
    return scan_pdf_page_multimodal_c_data_py(path, page_index)


def parse_pdf_page_table_layout_arrow_ipc(path, page_index, epsilon=2.0):
    return scan_pdf_page_table_layout_arrow_ipc_py(path, page_index, epsilon)


def parse_pdf_page_table_layout_c_data(path, page_index, epsilon=2.0):
    """Deprecated: prefer ``FastPdfPageScanner.get_page_table_batch``."""
    return scan_pdf_page_table_layout_c_data_py(
               path,
               page_index,
               epsilon=epsilon,
           )


def ocr_runs_record_batch(ocr_runs):
    """Build a PyArrow RecordBatch for ``*_from_ocr_*`` intake.

    Schema (required columns):
      * ``text``: utf8
      * ``px_x``: float32  — run left edge in raster pixels
      * ``px_y``: float32  — baseline y in raster pixels (top-down)
      * ``px_height``: float32

    Prefer this (or a ``pa.Table``) over a list of dicts / JSON. Rust keeps
    the batch columnar through core — no intermediate ``OcrTextRun`` vector
    on the Arrow happy path. Text is still cloned into cell assignments when
    a run routes into a table cell.
    """
    import pyarrow as pa

    if hasattr(ocr_runs, "schema") and (
        hasattr(ocr_runs, "__arrow_c_array__") or hasattr(ocr_runs, "_export_to_c")
    ):
        return ocr_runs

    texts = []
    px_x = []
    px_y = []
    px_height = []
    for idx, run in enumerate(ocr_runs):
        try:
            texts.append(run["text"])
            px_x.append(float(run["px_x"]))
            px_y.append(float(run["px_y"]))
            px_height.append(float(run["px_height"]))
        except KeyError as exc:
            raise ValueError(
                f"ocr_runs[{idx}] missing required key {exc.args[0]!r} "
                "(expected text/px_x/px_y/px_height)"
            ) from exc

    return pa.RecordBatch.from_arrays(
        [
            pa.array(texts, type=pa.string()),
            pa.array(px_x, type=pa.float32()),
            pa.array(px_y, type=pa.float32()),
            pa.array(px_height, type=pa.float32()),
        ],
        names=["text", "px_x", "px_y", "px_height"],
    )


def parse_pdf_page_table_layout_from_ocr_arrow_ipc(
    path, page_index, ocr_runs, epsilon=2.0, dpi=300.0
):
    return scan_pdf_page_table_layout_from_ocr_arrow_ipc_py(
        path, page_index, ocr_runs, epsilon, dpi
    )


def parse_pdf_page_table_layout_from_ocr_c_data(
    path, page_index, ocr_runs, epsilon=2.0, dpi=300.0
):
    """Deprecated: prefer
    ``FastPdfPageScanner.get_page_table_batch_from_ocr``."""
    return scan_pdf_page_table_layout_from_ocr_c_data_py(
               path,
               page_index,
               ocr_runs,
               epsilon=epsilon,
               dpi=dpi,
           )


def parse_pdf_page_table_arrow_ipc(path, page_index, epsilon=2.0):
    return scan_pdf_page_table_arrow_ipc_py(path, page_index, epsilon)


def parse_pdf_page_table_c_data(path, page_index, epsilon=2.0):
    """Deprecated: prefer ``FastPdfPageScanner.get_page_tables_batch``."""
    return scan_pdf_page_table_c_data_py(
               path,
               page_index,
               epsilon=epsilon,
           )


def parse_pdf_page_table_from_ocr_arrow_ipc(
    path, page_index, ocr_runs, epsilon=2.0, dpi=300.0
):
    return scan_pdf_page_table_from_ocr_arrow_ipc_py(
        path, page_index, ocr_runs, epsilon, dpi
    )


def parse_pdf_page_table_from_ocr_c_data(
    path, page_index, ocr_runs, epsilon=2.0, dpi=300.0
):
    """Deprecated: prefer
    ``FastPdfPageScanner.get_page_tables_batch_from_ocr``."""
    return scan_pdf_page_table_from_ocr_c_data_py(
               path,
               page_index,
               ocr_runs,
               epsilon=epsilon,
               dpi=dpi,
           )


def parse_pdf_page_count(path):
    return scan_pdf_page_count_py(path)


def parse_pdf_page_dimensions(path):
    """Return per-page CropBox/MediaBox geometry.

    Prefer ``FastPdfPageScanner.page_dimensions()`` when iterating pages —
    that path reuses the cached open. This free helper re-opens the PDF.

    Returns a list of ``PageDimension`` objects with fields
    ``page_index``, ``x0``, ``y0``, ``width``, ``height``, ``rotate``
    in PDF user-space points.
    """
    return PdfScanner(path).page_dimensions()


def release_arrow_c_data(array_ptr, schema_ptr):
    """Free Arrow C Data pointers that were not imported into PyArrow.

    Only needed for deprecated ``*_c_data`` APIs. Capsule paths
    (``*_arrow_c_array`` / ``FastPdfPageScanner``) own their buffers.
    """
    return release_arrow_c_data_py(array_ptr, schema_ptr)


def parse_pdf_page_bitmap_components(path, page_index):
    """Return connected glyph candidates and rough text-line bands.

    This is the native scan/fax fallback for PDFs with no semantic text
    layer. It decodes the page's embedded image/CCITT bitmap in Rust and
    returns ``None`` when the page has no extractable bitmap image.
    """
    raw = scan_pdf_page_bitmap_components_json_py(path, page_index)
    return json.loads(raw)


def parse_pdf_page_rasterized_png(path, page_index, dpi=180.0):
    return scan_pdf_page_rasterized_png_py(path, page_index, dpi)


def _import_record_batch_from_c(array_ptr, schema_ptr):
    """Import Arrow C Data pointers, freeing them if PyArrow import fails."""
    import pyarrow as pa

    try:
        batch = pa.RecordBatch._import_from_c(array_ptr, schema_ptr)
    except Exception as exc:
        try:
            release_arrow_c_data_py(array_ptr, schema_ptr)
        except Exception:  # noqa: BLE001,S110
            # Best-effort cleanup must not replace the original import error.
            pass
        raise RuntimeError(
            f"Failed to wrap Arrow C Data pointers from Rust: {exc}"
        ) from exc
    # The import moved the array and schema out of the two structs and marked
    # them released; what is left is the two boxes, freed here so that each
    # issued pair is freed exactly once.
    release_arrow_c_data_py(array_ptr, schema_ptr)
    return batch


def _import_record_batch_from_capsules(schema_capsule, array_capsule):
    """Import Arrow PyCapsules produced by ``*_arrow_c_array`` methods."""
    import pyarrow as pa

    return pa.RecordBatch._import_from_c_capsule(schema_capsule, array_capsule)


def parse_pdf_text(path, return_diagnostics: bool = False, reading_order: bool = False):
    """Extract every native text run from ``path`` as a single string.

    The decoder picks the most accurate path available per font:
    `/ToUnicode` CMap → Type0 UTF-16 BE → `/Encoding /Differences` →
    named encoding → Standard-14 implicit → raw-byte Latin-1 fallback.

    No OCR is invoked.  Pages are separated by ``\\f`` (form-feed) and runs
    within a page by newlines.  Returns the text by default; pass
    ``return_diagnostics=True`` to also receive a ``FullTextDiagnostics``
    object with per-decoding-path counters and a fallback budget — use the
    fallback budget to decide whether to escalate to OCR.

    Pass ``reading_order=True`` to re-emit each page in the order chosen by
    ``layout-fast-core``'s spectral engine after coalescing runs into
    paragraphs.  The producer's PDF emission order is correct for most
    single-column and clean 2-column-with-gutter layouts (where this flag is
    a no-op); on multi-column papers with sidebars / banners the spectral
    pass surfaces the column-major reading order that the raw stream order
    interleaves.  The flag is opt-in because most callers are downstream of
    a 90 %+ accurate emission baseline; it pays off on the ~10 % of pages
    where reading order is non-trivial.  Falls back to emission order if
    the engine misbehaves so the function never returns empty-handed.

    Examples
    --------
    >>> parse_pdf_text("contract.pdf")
    'CONTRATO DE PRESTAÇÃO DE SERVIÇOS\\n...'
    >>> text, diag = parse_pdf_text("contract.pdf", return_diagnostics=True)
    >>> diag.fallback_chars / max(diag.chars, 1) < 0.05  # less than 5% garbled
    True
    >>> parse_pdf_text("two_column_paper.pdf", reading_order=True)
    'Abstract\\nWe propose…'
    """
    text, diag = scan_pdf_full_text_py(path, reading_order)
    if return_diagnostics:
        return text, diag
    return text


def parse_pdf_text_bytes(
    source: bytes, return_diagnostics: bool = False, reading_order: bool = False
):
    """Extract native PDF text directly from bytes without filesystem transport."""
    text, diag = scan_pdf_full_text_bytes_py(source, reading_order)
    if return_diagnostics:
        return text, diag
    return text


class FastPdfPageScanner:
    """Page-level scanner that opens the PDF once and reuses the parse.

    Prefer this over the free ``parse_pdf_page_*`` helpers when iterating
    pages — those re-open the document on every call.

    Page batches are returned via Arrow PyCapsules
    (``RecordBatch._import_from_c_capsule``). Deprecated raw ``*_c_data``
    pointer helpers remain on the free-function surface for compat.
    """

    def __init__(self, path):
        self.path = path
        self._scanner = PdfScanner(path)

    def page_count(self):
        return self._scanner.page_count

    def page_dimensions(self):
        return self._scanner.page_dimensions()

    def get_page_batch(self, page_index):
        schema_capsule, array_capsule = self._scanner.page_multimodal_arrow_c_array(
            page_index
        )
        return _import_record_batch_from_capsules(schema_capsule, array_capsule)

    def get_page_table_batch(self, page_index, epsilon=2.0):
        schema_capsule, array_capsule = self._scanner.page_table_layout_arrow_c_array(
            page_index, epsilon
        )
        return _import_record_batch_from_capsules(schema_capsule, array_capsule)

    def get_page_table_batch_from_ocr(
        self, page_index, ocr_runs, epsilon=2.0, dpi=300.0
    ):
        schema_capsule, array_capsule = (
            self._scanner.page_table_layout_from_ocr_arrow_c_array(
                page_index, ocr_runs, epsilon, dpi
            )
        )
        return _import_record_batch_from_capsules(schema_capsule, array_capsule)

    def get_page_tables_batch(self, page_index, epsilon=2.0):
        schema_capsule, array_capsule = self._scanner.page_table_arrow_c_array(
            page_index, epsilon
        )
        return _import_record_batch_from_capsules(schema_capsule, array_capsule)

    def get_page_tables_batch_from_ocr(
        self, page_index, ocr_runs, epsilon=2.0, dpi=300.0
    ):
        schema_capsule, array_capsule = self._scanner.page_table_from_ocr_arrow_c_array(
            page_index, ocr_runs, epsilon, dpi
        )
        return _import_record_batch_from_capsules(schema_capsule, array_capsule)

    def stream_pages(self):
        """Yield multimodal page ``RecordBatch``es via capsules
        (one per page)."""
        for page_index in range(self.page_count()):
            yield self.get_page_batch(page_index)

    def stream_page_batches(self):
        """Alias for :meth:`stream_pages` — Arrow page multimodal stream.

        Yields one ``RecordBatch`` per page imported from PyCapsules. Prefer
        this name when treating the scanner as an Arrow producer.
        """
        yield from self.stream_pages()


__all__ = [
    "FastPdfPageScanner",
    "FullTextDiagnostics",
    "PageDimension",
    "PdfScanner",
    "ocr_runs_record_batch",
    "parse_pdf",
    "parse_pdf_json",
    "parse_pdf_page_bitmap_components",
    "parse_pdf_page_count",
    "parse_pdf_page_dimensions",
    "parse_pdf_page_multimodal_arrow_ipc",
    "parse_pdf_page_multimodal_c_data",
    "parse_pdf_page_rasterized_png",
    "parse_pdf_page_table_arrow_ipc",
    "parse_pdf_page_table_c_data",
    "parse_pdf_page_table_from_ocr_arrow_ipc",
    "parse_pdf_page_table_from_ocr_c_data",
    "parse_pdf_page_table_layout_arrow_ipc",
    "parse_pdf_page_table_layout_c_data",
    "parse_pdf_page_table_layout_from_ocr_arrow_ipc",
    "parse_pdf_page_table_layout_from_ocr_c_data",
    "parse_pdf_paragraphs",
    "parse_pdf_semantic_runs_arrow_ipc",
    "parse_pdf_semantic_runs_json",
    "parse_pdf_text",
    "parse_pdf_text_bytes",
    "release_arrow_c_data",
]

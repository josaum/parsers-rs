# pdf-fast-write

Sovereign PDF page subsetter powered by Rust. Extracts pages from a PDF into a
new PDF with **zero external dependencies** (no PyMuPDF, no pypdf).

```python
from pdf_fast_write import extract_pages

extract_pages("input.pdf", [1, 2, 3, 5], "subset.pdf")
```

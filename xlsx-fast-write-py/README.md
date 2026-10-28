# xlsx-fast-write

Sovereign XLSX writer powered by Rust. Generates `.xlsx` files with **zero
external dependencies** (no `openpyxl`, no `rust_xlsxwriter`).

```python
from xlsx_fast_write import write_xlsx

write_xlsx({
    "sheets": [{
        "name": "Summary",
        "column_widths": {"0": 12.0},
        "rows": [
            [{"t": "s", "v": "Page", "fmt": "header"}, {"t": "s", "v": "Type", "fmt": "header"}],
            [{"t": "n", "v": 1}, {"t": "s", "v": "Digital"}],
        ],
    }]
}, "out.xlsx")
```

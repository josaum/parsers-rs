# xlsx-fast-write-node

Sovereign XLSX writer with a native Rust core. No external dependencies.

```javascript
const { writeXlsx } = require('xlsx-fast-write-node');
writeXlsx({ sheets: [{ name: 'S', rows: [[{ t: 's', v: 'Hi' }]] }] }, 'out.xlsx');
```

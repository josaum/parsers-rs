# pdf-fast-write-node

Sovereign PDF page subsetter with a native Rust core. No external dependencies.

```javascript
const { extractPages } = require('pdf-fast-write-node');
extractPages('input.pdf', [1, 2, 3, 5], 'subset.pdf');
```

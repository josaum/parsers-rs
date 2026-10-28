# pdf-fast-node

High-performance PDF parser/table/text N-API bindings, full parity with `pdf-fast-py` using Arrow IPC Buffers.

Native N-API binding over the [`pdf-fast-core`](../pdf-fast-core) Rust crate.

## Install / build

These packages ship a prebuilt host-arch `pdf_fast_node.node`. To (re)build locally from the workspace root:

```bash
./scripts/build-node.sh
```

## Usage

```js
const m = require('pdf-fast-node');
```

See [`index.d.ts`](./index.d.ts) for the full typed surface.

## Notes

- Object inputs accept both `camelCase` and Python-style `snake_case` keys where applicable.
- Arrow data is returned as **IPC `Buffer`s** (consume with the `apache-arrow` package), not raw C-Data pointers.

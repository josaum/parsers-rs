# geo-fast-node

High-performance GIS N-API bindings, full parity with `geo-fast-py`.

## Build

```bash
make node-build NODE_CRATES=geo-fast-node
```

## Usage

```js
const geo = require('geo-fast-node');
const records = JSON.parse(geo.parseGeojsonToRecordsJson('{"type":"FeatureCollection","features":[]}'));
```

See `index.d.ts` for the typed surface.

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

## GeoTIFF, NDVI and spectral angle

```js
const geo = require('geo-fast-node');

// Georeferencing of a GeoTIFF (see `GeoTiffMetadata` in index.d.ts).
const meta = JSON.parse(geo.geotiffMetadataJson('scene.tif'));
console.log(meta.width, meta.height, meta.epsg_code, meta.pixel_is_point);

// One Float32Array per band. 8-bit samples are scaled to 0..1.
const bands = geo.geotiffBands('scene.tif');
// The same, refusing a raster whose bands would take more than 64 MiB.
const limited = geo.geotiffBands('scene.tif', 64 * 1024 * 1024);

console.log(geo.computeNdvi([0.5, 0.2], [0.1, 0.2]));   // Float32Array [0.666..., 0]
console.log(geo.spectralAngle([1, 0], [0, 1]));          // 1.5707...
```

A missing or unreadable file, an exceeded band limit, and mismatched lengths
are thrown as JavaScript errors. A panic in the Rust core is also thrown as
an error; it does not end the process.

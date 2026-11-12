# geo-fast

Thin Python bindings for [`geo-fast-core`](../geo-fast-core): GeoJSON /
shapefile → WKT records, spherical geodesic bbox area/IoU, antimeridian
split, and an STR-packed R-tree.

```bash
cd parsers-rs/geo-fast-py
maturin develop
```

```python
from geo_fast import parse_geojson_records, strtree_query, bbox_iou_geodesic

recs = parse_geojson_records(
    '{"type":"Feature","geometry":{"type":"Point","coordinates":[-47.9,-15.8]},"properties":{}}'
)
assert recs[0]["wkt"].startswith("POINT")

hits = strtree_query([[0, 0, 1, 1], [10, 10, 11, 11]], "0,0,0.5,0.5")
assert hits == [0]
```

GeoTIFF write / full PROJ CRS transforms remain deferred in the Rust core.
ENVI complex pixel types remain deferred (fail loud); I64/U64 DN reads are supported.

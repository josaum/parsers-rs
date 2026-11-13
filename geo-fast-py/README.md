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

## GeoTIFF, NDVI and spectral angle

```python
import geo_fast

# Georeferencing of a GeoTIFF: size, CRS, corners, raster type and GeoKeys.
meta = geo_fast.geotiff_metadata("scene.tif")
print(meta["width"], meta["height"], meta["band_count"], meta["epsg_code"])
print(meta["pixel_is_point"])   # True when a tiepoint ties a pixel centre
print(meta["upper_left"], meta["lower_right"])   # in the raster's own CRS
print(meta["bbox"])             # degrees; None for a projected raster
print(meta["geokey_params"])    # {"doubles": {...}, "strings": {...}} by GeoKey ID

# Every band as a list of samples. 8-bit samples are scaled to 0..1.
bands = geo_fast.geotiff_bands("scene.tif")
# The same, refusing a raster whose bands would take more than 64 MiB.
bands = geo_fast.geotiff_bands("scene.tif", max_band_bytes=64 * 1024 * 1024)

# NDVI per sample: (nir - red) / (nir + red).
print(geo_fast.compute_ndvi([0.5, 0.2], [0.1, 0.2]))   # [0.666..., 0.0]

# Spectral angle in radians between a pixel spectrum and a reference.
print(geo_fast.spectral_angle([1.0, 0.0], [0.0, 1.0]))  # 1.5707...
```

A missing or unreadable file, a band limit that is exceeded, and bands or
spectra of different lengths each raise `RuntimeError`. Every function
releases the GIL while the Rust core runs.

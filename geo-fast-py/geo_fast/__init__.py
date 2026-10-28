"""geo-fast: GIS helpers powered by geo-fast-core (Rust).

Thin Geospatial surface: GeoJSON/shapefile → WKT records, spherical
geodesic bbox area/IoU, antimeridian split, and STRtree queries.
"""

from __future__ import annotations

import json
from typing import Any

from geo_fast._core import (
    bbox_area_km2_py,
    bbox_iou_geodesic_py,
    parse_geojson_to_records_json,
    parse_shapefile_to_records_json,
    split_bbox_antimeridian_json,
    strtree_query_json,
    version,
)

__version__ = version()


def parse_geojson_records(geojson: str) -> list[dict[str, Any]]:
    """Parse GeoJSON text into `{id, wkt, lon, lat, properties}` dicts."""
    return json.loads(parse_geojson_to_records_json(geojson))


def parse_shapefile_records(path: str) -> list[dict[str, Any]]:
    """Parse a `.shp` path into the same record dicts (`.dbf` inferred)."""
    return json.loads(parse_shapefile_to_records_json(path))


def bbox_iou_geodesic(a: str, b: str) -> float:
    """Spherical geodesic IoU for two WMS bbox strings."""
    return float(bbox_iou_geodesic_py(a, b))


def bbox_area_km2(bbox: str) -> float:
    """Spherical geodesic area (km²) for a WMS bbox string."""
    return float(bbox_area_km2_py(bbox))


def split_bbox_antimeridian(bbox: str) -> list[list[float]]:
    """Split a (possibly wrapping) WMS bbox into non-wrapping parts."""
    return json.loads(split_bbox_antimeridian_json(bbox))


def strtree_query(
    boxes: list[list[float]] | list[tuple[float, float, float, float]],
    query_bbox: str,
) -> list[int]:
    """STRtree query: `boxes` are `[min_lon, min_lat, max_lon, max_lat]`."""
    return json.loads(strtree_query_json(json.dumps(boxes), query_bbox))


__all__ = [
    "__version__",
    "parse_geojson_records",
    "parse_shapefile_records",
    "bbox_iou_geodesic",
    "bbox_area_km2",
    "split_bbox_antimeridian",
    "strtree_query",
]

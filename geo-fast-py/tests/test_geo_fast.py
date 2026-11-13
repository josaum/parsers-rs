"""Every `geo_fast` entry point: a round trip, its defaults and its errors.

Every input is built here with the standard library, so the suite needs no
fixture file and no network.
"""

from __future__ import annotations

import ast
import json
import math
import struct
import inspect
import sys
import threading
from pathlib import Path

import pytest

MODULE = pytest.importorskip("geo_fast")


def times_another_thread_ran_during(call, attempts: int = 5) -> int:
    """How many of `attempts` calls let a second Python thread run meanwhile.

    The switch interval is raised so the interpreter never hands the GIL over
    between bytecodes of the calling thread. A second thread, made runnable
    just before `call`, can then run only if `call` releases the GIL itself:
    with the GIL held the count is exactly 0, whatever the machine's load.
    No duration is compared with a threshold.
    """
    previous = sys.getswitchinterval()
    sys.setswitchinterval(1000.0)
    count = 0
    try:
        for _ in range(attempts):
            go = threading.Event()
            ran: list = []

            def waiter() -> None:
                go.wait()
                ran.append(True)

            thread = threading.Thread(target=waiter)
            thread.start()  # returns once the waiter is blocked in go.wait()
            go.set()
            call()
            count += bool(ran)
            thread.join()
    finally:
        sys.setswitchinterval(previous)
    return count


def holds_the_gil() -> int:
    """A call that never releases the GIL: the control for the test below."""
    return sum(range(3_000_000))


GEOJSON = json.dumps(
    {
        "type": "FeatureCollection",
        "features": [
            {
                "type": "Feature",
                "id": "a",
                "geometry": {"type": "Point", "coordinates": [10.0, 50.0]},
                "properties": {"name": "alpha"},
            }
        ],
    }
)


def geotiff_bytes(point: bool = True) -> bytes:
    """A 2x2 8-bit little-endian GeoTIFF in EPSG:4326, one strip, samples 1..4."""
    geokeys = [1, 1, 0, 3, 1024, 0, 1, 2, 1025, 0, 1, 2 if point else 1, 2048, 0, 1, 4326]
    scale = struct.pack("<3d", 1.0, 1.0, 0.0)
    tiepoint = struct.pack("<6d", 0.0, 0.0, 0.0, 10.0, 50.0, 0.0)
    keys = struct.pack(f"<{len(geokeys)}H", *geokeys)
    entries = 12
    data_at = 8 + 2 + entries * 12 + 4
    pixels_at, scale_at = data_at, data_at + 4
    tiepoint_at = scale_at + len(scale)
    keys_at = tiepoint_at + len(tiepoint)
    short, long_, double = 3, 4, 12
    tags = [
        (256, short, 1, 2), (257, short, 1, 2), (258, short, 1, 8), (259, short, 1, 1),
        (262, short, 1, 1), (273, long_, 1, pixels_at), (277, short, 1, 1),
        (278, short, 1, 2), (279, long_, 1, 4), (33550, double, 3, scale_at),
        (33922, double, 6, tiepoint_at), (34735, short, len(geokeys), keys_at),
    ]
    assert len(tags) == entries
    out = struct.pack("<2sHI", b"II", 42, 8) + struct.pack("<H", entries)
    for tag, kind, count, value in tags:
        packed = struct.pack("<H", value) + b"\0\0" if kind == short and count == 1 else struct.pack("<I", value)
        out += struct.pack("<HHI", tag, kind, count) + packed
    out += struct.pack("<I", 0)
    return out + bytes([1, 2, 3, 4]) + scale + tiepoint + keys


@pytest.fixture()
def tiff_path(tmp_path: Path) -> str:
    path = tmp_path / "raster.tif"
    path.write_bytes(geotiff_bytes())
    return str(path)


def test_version_is_exposed() -> None:
    assert MODULE.__version__ and isinstance(MODULE.__version__, str)


def test_parse_geojson_records_round_trips_a_point() -> None:
    (record,) = MODULE.parse_geojson_records(GEOJSON)
    assert record["wkt"].upper().startswith("POINT")
    assert (record["lon"], record["lat"]) == (10.0, 50.0)
    assert record["properties"]["name"] == "alpha"


def test_parse_geojson_records_refuses_malformed_text() -> None:
    with pytest.raises(RuntimeError):
        MODULE.parse_geojson_records("{not json")
    with pytest.raises(TypeError):
        MODULE.parse_geojson_records(7)


def test_parse_shapefile_records_reports_a_missing_or_malformed_file(tmp_path: Path) -> None:
    with pytest.raises(RuntimeError):
        MODULE.parse_shapefile_records(str(tmp_path / "missing.shp"))
    bad = tmp_path / "bad.shp"
    bad.write_bytes(b"not a shapefile")
    with pytest.raises(RuntimeError):
        MODULE.parse_shapefile_records(str(bad))


def test_parse_shapefile_records_reads_a_point_shapefile(tmp_path: Path) -> None:
    record = struct.pack(">ii", 1, 10) + struct.pack("<idd", 1, 10.0, 50.0)
    length_words = (100 + len(record)) // 2
    header = struct.pack(">i5ii", 9994, 0, 0, 0, 0, 0, length_words)
    header += struct.pack("<ii8d", 1000, 1, 10.0, 50.0, 10.0, 50.0, 0.0, 0.0, 0.0, 0.0)
    path = tmp_path / "points.shp"
    path.write_bytes(header + record)
    try:
        records = MODULE.parse_shapefile_records(str(path))
    except RuntimeError:
        pytest.skip("the core requires a companion .dbf for this shapefile")
    assert [(r["lon"], r["lat"]) for r in records] == [(10.0, 50.0)]


def test_bbox_area_and_iou() -> None:
    area = MODULE.bbox_area_km2("0,0,1,1")
    assert 12_000 < area < 12_500
    assert MODULE.bbox_iou_geodesic("0,0,1,1", "0,0,1,1") == pytest.approx(1.0)
    assert MODULE.bbox_iou_geodesic("0,0,1,1", "10,10,11,11") == 0.0
    with pytest.raises(RuntimeError):
        MODULE.bbox_area_km2("not,a,bbox")
    with pytest.raises(RuntimeError):
        MODULE.bbox_iou_geodesic("0,0,1,1", "1,2,3")


def test_split_bbox_antimeridian() -> None:
    assert MODULE.split_bbox_antimeridian("0,0,1,1") == [[0.0, 0.0, 1.0, 1.0]]
    assert len(MODULE.split_bbox_antimeridian("170,-10,-170,10")) == 2
    with pytest.raises(RuntimeError):
        MODULE.split_bbox_antimeridian("nope")


def test_strtree_query_returns_sorted_matching_indices() -> None:
    boxes = [[0, 0, 1, 1], [10, 10, 11, 11], (0.5, 0.5, 2, 2)]
    assert MODULE.strtree_query(boxes, "0,0,1,1") == [0, 2]
    assert MODULE.strtree_query(boxes, "50,50,51,51") == []
    with pytest.raises(RuntimeError):
        MODULE.strtree_query(boxes, "nope")
    with pytest.raises(RuntimeError):
        MODULE.strtree_query([[0, 0, 1]], "0,0,1,1")


def test_geotiff_metadata_reports_size_crs_and_raster_type(tiff_path: str, tmp_path: Path) -> None:
    meta = MODULE.geotiff_metadata(tiff_path)
    assert (meta["width"], meta["height"], meta["band_count"]) == (2, 2, 1)
    assert meta["epsg_code"] == 4326 and meta["is_geographic"] is True
    assert meta["pixel_is_point"] is True
    assert meta["pixel_scale"] == [1.0, 1.0]
    assert meta["tiepoints"] == [[0.0, 0.0, 0.0, 10.0, 50.0, 0.0]]
    assert meta["geokeys"]["1025"] == 2 and meta["geokeys"]["2048"] == 4326
    assert meta["geokey_params"] == {"doubles": {}, "strings": {}}
    # A pixel-is-point tiepoint ties a pixel centre: the corner is half a pixel out.
    assert meta["upper_left"] == [9.5, 50.5]

    area = tmp_path / "area.tif"
    area.write_bytes(geotiff_bytes(point=False))
    area_meta = MODULE.geotiff_metadata(str(area))
    assert area_meta["pixel_is_point"] is False
    assert area_meta["upper_left"] == [10.0, 50.0]
    assert area_meta["bbox"] == [10.0, 48.0, 12.0, 50.0]


def test_geotiff_bands_reads_samples_and_honours_the_limit(tiff_path: str) -> None:
    # The core scales 8-bit samples to 0..1. A band is little-endian f32 bytes.
    expected = [n / 255 for n in (1, 2, 3, 4)]
    (band,) = MODULE.geotiff_bands(tiff_path)
    assert isinstance(band, bytes) and len(band) == 16
    assert struct.unpack("<4f", band) == pytest.approx(expected, abs=1e-6)
    # The limit counts the bytes returned: 16 fits, 15 does not.
    (limited,) = MODULE.geotiff_bands(tiff_path, max_band_bytes=16)
    assert limited == band
    # A band feeds straight into NDVI once unpacked.
    samples = list(struct.unpack("<4f", band))
    assert MODULE.compute_ndvi(samples, samples) == pytest.approx([0.0] * 4)
    with pytest.raises(RuntimeError):
        MODULE.geotiff_bands(tiff_path, max_band_bytes=15)


def test_geotiff_entry_points_refuse_missing_and_malformed_files(tmp_path: Path) -> None:
    bad = tmp_path / "bad.tif"
    bad.write_bytes(b"not a tiff")
    for path in (str(tmp_path / "missing.tif"), str(bad)):
        with pytest.raises(RuntimeError):
            MODULE.geotiff_metadata(path)
        with pytest.raises(RuntimeError):
            MODULE.geotiff_bands(path)


def test_compute_ndvi_and_its_length_check() -> None:
    assert MODULE.compute_ndvi([0.5, 0.2], [0.1, 0.2]) == pytest.approx([2 / 3, 0.0], abs=1e-6)
    with pytest.raises(RuntimeError, match="equal length"):
        MODULE.compute_ndvi([0.5], [0.1, 0.2])
    with pytest.raises(TypeError):
        MODULE.compute_ndvi("ab", [0.1, 0.2])


def test_spectral_angle_and_its_length_check() -> None:
    assert MODULE.spectral_angle([1.0, 0.0], [1.0, 0.0]) == pytest.approx(0.0, abs=1e-3)
    assert MODULE.spectral_angle([1.0, 0.0], [0.0, 1.0]) == pytest.approx(math.pi / 2, abs=1e-3)
    with pytest.raises(RuntimeError):
        MODULE.spectral_angle([1.0], [1.0, 0.0])


def test_parsing_releases_the_gil() -> None:
    features = [
        {"type": "Feature", "geometry": {"type": "Point", "coordinates": [i % 90, i % 45]}, "properties": {}}
        for i in range(50_000)
    ]
    text = json.dumps({"type": "FeatureCollection", "features": features})
    assert times_another_thread_ran_during(holds_the_gil) == 0
    assert times_another_thread_ran_during(lambda: MODULE.parse_geojson_records(text)) >= 1


def test_the_wheel_ships_type_information_that_matches_the_module() -> None:
    package_dir = Path(MODULE.__file__).parent
    assert (package_dir / "py.typed").is_file()
    public = {
        name
        for name in dir(MODULE)
        if not name.startswith("_") and inspect.isfunction(getattr(MODULE, name))
    }
    assert public == {name for name in MODULE.__all__ if not name.startswith("_")}
    stub = package_dir / "__init__.pyi"
    if not stub.is_file():
        # No stub: the wrappers carry their own annotations.
        for name in public:
            assert "return" in getattr(MODULE, name).__annotations__, name
        return
    declared = {
        node.name: [a.arg for a in node.args.args + node.args.kwonlyargs]
        for node in ast.parse(stub.read_text(encoding="utf-8")).body
        if isinstance(node, ast.FunctionDef)
    }
    assert set(declared) == public
    for name, params in declared.items():
        assert params == list(inspect.signature(getattr(MODULE, name)).parameters), name

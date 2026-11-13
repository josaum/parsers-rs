//! Throughput of the parsers and the spatial index on inputs built in memory.
//!
//! Run with `cargo bench -p geo-fast-core --bench parse`. Three groups:
//!
//! - `shapefile`: 100,000 points with a two-field attribute table, and
//!   10,000 polygons of 64 vertices each.
//! - `geojson`: one `FeatureCollection` of at least 50 MB.
//! - `spatial_index`: a bulk load of 100,000 boxes and 10,000 queries.

use std::fmt::Write as _;
use std::hint::black_box;
use std::time::Duration;

use criterion::{criterion_group, criterion_main, Criterion, Throughput};
use geo_fast_core::{parse_geojson_str, parse_shapefile_bytes, BBox, SpatialIndex};

/// Points in the point shapefile.
const POINTS: usize = 100_000;
/// Polygons in the polygon shapefile.
const POLYGONS: usize = 10_000;
/// Vertices of each polygon's one ring, before the vertex that closes it.
const POLYGON_VERTICES: usize = 64;
/// Least size of the GeoJSON document, in bytes.
const GEOJSON_BYTES: usize = 50 * 1024 * 1024;
/// Boxes loaded into the spatial index.
const INDEX_BOXES: usize = 100_000;
/// Queries run against the spatial index.
const INDEX_QUERIES: usize = 10_000;

/// A deterministic value in `0.0..1.0` for `seed` (SplitMix64).
fn unit(seed: u64) -> f64 {
    let mut z = seed.wrapping_add(0x9E37_79B9_7F4A_7C15);
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    ((z ^ (z >> 31)) >> 11) as f64 / (1u64 << 53) as f64
}

/// A longitude and latitude for `seed`.
fn position(seed: u64) -> (f64, f64) {
    (
        unit(seed * 2) * 360.0 - 180.0,
        unit(seed * 2 + 1) * 180.0 - 90.0,
    )
}

/// The 100-byte header of a `.shp` file of `shape_type`.
fn shp_header(shape_type: i32) -> Vec<u8> {
    let mut shp = Vec::new();
    shp.extend_from_slice(&9994i32.to_be_bytes());
    shp.extend_from_slice(&[0u8; 20]);
    shp.extend_from_slice(&0i32.to_be_bytes()); // file length, not read
    shp.extend_from_slice(&1000i32.to_le_bytes());
    shp.extend_from_slice(&shape_type.to_le_bytes());
    for edge in [-180.0f64, -90.0, 180.0, 90.0] {
        shp.extend_from_slice(&edge.to_le_bytes());
    }
    shp.extend_from_slice(&[0u8; 32]); // Z and M ranges
    shp
}

/// Appends one record: its number, its length in 16-bit words, its content.
fn push_record(shp: &mut Vec<u8>, number: usize, content: &[u8]) {
    shp.extend_from_slice(&(number as i32).to_be_bytes());
    shp.extend_from_slice(&((content.len() / 2) as i32).to_be_bytes());
    shp.extend_from_slice(content);
}

/// A point shapefile and its attribute table: a name and a number per point.
fn point_shapefile() -> (Vec<u8>, Vec<u8>) {
    let mut shp = shp_header(1);
    for i in 0..POINTS {
        let (lon, lat) = position(i as u64);
        let mut content = Vec::with_capacity(20);
        content.extend_from_slice(&1i32.to_le_bytes());
        content.extend_from_slice(&lon.to_le_bytes());
        content.extend_from_slice(&lat.to_le_bytes());
        push_record(&mut shp, i + 1, &content);
    }

    // (name, type, width)
    let fields: [(&str, u8, u8); 2] = [("NAME", b'C', 16), ("VALUE", b'N', 10)];
    let record_size: u16 = 1 + fields.iter().map(|f| u16::from(f.2)).sum::<u16>();
    let header_size = (32 + 32 * fields.len() + 1) as u16;
    let mut dbf = vec![3u8, 26, 1, 1];
    dbf.extend_from_slice(&(POINTS as u32).to_le_bytes());
    dbf.extend_from_slice(&header_size.to_le_bytes());
    dbf.extend_from_slice(&record_size.to_le_bytes());
    dbf.extend_from_slice(&[0u8; 20]);
    for (name, field_type, width) in fields {
        let mut descriptor = [0u8; 32];
        descriptor[..name.len()].copy_from_slice(name.as_bytes());
        descriptor[11] = field_type;
        descriptor[16] = width;
        dbf.extend_from_slice(&descriptor);
    }
    dbf.push(0x0D);
    for i in 0..POINTS {
        dbf.push(b' ');
        dbf.extend_from_slice(format!("{:<16}", format!("station {i}")).as_bytes());
        dbf.extend_from_slice(format!("{:>10}", i * 7).as_bytes());
    }
    (shp, dbf)
}

/// A polygon shapefile: one closed ring of [`POLYGON_VERTICES`] per record.
fn polygon_shapefile() -> Vec<u8> {
    let mut shp = shp_header(5);
    let n_points = POLYGON_VERTICES + 1;
    for i in 0..POLYGONS {
        let (lon, lat) = position(i as u64);
        let mut content = Vec::with_capacity(48 + 16 * n_points);
        content.extend_from_slice(&5i32.to_le_bytes());
        for edge in [lon - 0.5, lat - 0.5, lon + 0.5, lat + 0.5] {
            content.extend_from_slice(&edge.to_le_bytes());
        }
        content.extend_from_slice(&1i32.to_le_bytes()); // parts
        content.extend_from_slice(&(n_points as i32).to_le_bytes());
        content.extend_from_slice(&0i32.to_le_bytes()); // start of the part
        for vertex in 0..n_points {
            let angle = (vertex % POLYGON_VERTICES) as f64 / POLYGON_VERTICES as f64
                * std::f64::consts::TAU;
            // Clockwise, as the format has an outer ring.
            content.extend_from_slice(&(lon + 0.5 * angle.cos()).to_le_bytes());
            content.extend_from_slice(&(lat - 0.5 * angle.sin()).to_le_bytes());
        }
        push_record(&mut shp, i + 1, &content);
    }
    shp
}

/// A `FeatureCollection` of polygons with properties, at least
/// [`GEOJSON_BYTES`] long.
fn geojson_document() -> String {
    let mut json = String::with_capacity(GEOJSON_BYTES + 4096);
    json.push_str(r#"{"type":"FeatureCollection","features":["#);
    let mut i = 0u64;
    while json.len() < GEOJSON_BYTES {
        if i > 0 {
            json.push(',');
        }
        let (lon, lat) = position(i);
        write!(
            json,
            r#"{{"type":"Feature","id":{i},"properties":{{"name":"parcel {i}","area":{:.3},"zone":"R{}","tags":["a","b"],"owner":{{"kind":"private","since":{}}}}},"geometry":{{"type":"Polygon","coordinates":[["#,
            unit(i) * 1000.0,
            i % 7,
            1950 + i % 70,
        )
        .expect("write to a String");
        for vertex in 0..=16u64 {
            if vertex > 0 {
                json.push(',');
            }
            let angle = (vertex % 16) as f64 / 16.0 * std::f64::consts::TAU;
            write!(
                json,
                "[{:.6},{:.6}]",
                lon + 0.01 * angle.cos(),
                lat + 0.01 * angle.sin()
            )
            .expect("write to a String");
        }
        json.push_str("]]}}");
        i += 1;
    }
    json.push_str("]}");
    json
}

/// Boxes of up to half a degree for the index, with their ids.
fn index_boxes() -> Vec<(BBox, usize)> {
    (0..INDEX_BOXES)
        .map(|i| {
            let (lon, lat) = position(i as u64);
            let (w, h) = (
                unit(i as u64 + 7_000_000) * 0.5,
                unit(i as u64 + 9_000_000) * 0.5,
            );
            let (lon, lat) = (lon.min(179.0), lat.min(89.0));
            (BBox::new(lon, lat, lon + w, lat + h), i)
        })
        .collect()
}

/// Query boxes of two degrees a side.
fn index_queries() -> Vec<BBox> {
    (0..INDEX_QUERIES)
        .map(|i| {
            let (lon, lat) = position(i as u64 + 50_000_000);
            let (lon, lat) = (lon.min(177.0), lat.min(87.0));
            BBox::new(lon, lat, lon + 2.0, lat + 2.0)
        })
        .collect()
}

fn shapefile(c: &mut Criterion) {
    let (points, attributes) = point_shapefile();
    let polygons = polygon_shapefile();
    let mut group = c.benchmark_group("shapefile");
    group.throughput(Throughput::Bytes((points.len() + attributes.len()) as u64));
    group.bench_function("points_100k", |b| {
        b.iter(|| parse_shapefile_bytes(black_box(&points), Some(black_box(&attributes))))
    });
    group.throughput(Throughput::Bytes(polygons.len() as u64));
    group.bench_function("polygons_10k", |b| {
        b.iter(|| parse_shapefile_bytes(black_box(&polygons), None))
    });
    group.finish();
}

fn geojson(c: &mut Criterion) {
    let document = geojson_document();
    let mut group = c.benchmark_group("geojson");
    group.throughput(Throughput::Bytes(document.len() as u64));
    group.bench_function("feature_collection_50mb", |b| {
        b.iter(|| parse_geojson_str(black_box(&document)))
    });
    group.finish();
}

fn spatial_index(c: &mut Criterion) {
    let boxes = index_boxes();
    let queries = index_queries();
    let index = SpatialIndex::bulk_load(boxes.iter().copied());
    let mut group = c.benchmark_group("spatial_index");
    group.bench_function("bulk_load_100k", |b| {
        b.iter(|| SpatialIndex::bulk_load(black_box(&boxes).iter().copied()))
    });
    group.bench_function("query_10k", |b| {
        let mut hits = Vec::new();
        b.iter(|| {
            let mut total = 0usize;
            for query in &queries {
                hits.clear();
                index.query_into(black_box(query), &mut hits);
                total += hits.len();
            }
            total
        })
    });
    group.finish();
}

criterion_group! {
    name = benches;
    // The inputs are large and the machine is shared: few samples, each of
    // whole parses, keep a run to about a minute.
    config = Criterion::default()
        .sample_size(10)
        .warm_up_time(Duration::from_secs(1))
        .measurement_time(Duration::from_secs(5));
    targets = shapefile, geojson, spatial_index
}
criterion_main!(benches);

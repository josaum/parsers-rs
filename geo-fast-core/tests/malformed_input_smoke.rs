//! Malformed geospatial files must be rejected with an error.
//!
//! Every reader in this crate is handed bytes from files nobody vetted. A bad
//! file may produce an `Err`; it must never panic, overflow the stack, loop
//! without consuming input, or allocate more than its own size can justify.
//!
//! The file has three parts: builders for small, valid seed files; one test
//! per defect that was found, each built from the smallest input that showed
//! it; and a mutation run per format that starts from the valid seeds.

#[path = "../../tools/mutation_smoke.rs"]
mod smoke;

use std::alloc::{GlobalAlloc, Layout};
use std::cell::Cell;
use std::io::{Cursor, Write};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Mutex, PoisonError};
use std::time::Duration;

use geo_fast_core::geojson::{features_to_geojson, parse_geojson_str};
use geo_fast_core::geotiff::{parse_geotiff_bands, parse_geotiff_metadata_reader};
use geo_fast_core::hyperspectral::HyperspectralParser;
use geo_fast_core::sar::SarParser;
use geo_fast_core::shapefile::parse_shapefile_bytes;
use geo_fast_core::thermal::ThermalParser;
use geo_fast_core::{geometry_area_m2, BBox, GeoFeature, Geometry, SpatialIndex};

// ── Test plumbing ────────────────────────────────────────────────────────────

thread_local! {
    // Const-initialized and without destructors, so touching them from inside
    // the allocator neither allocates nor registers a thread-exit callback.
    static LIVE_BYTES: Cell<usize> = const { Cell::new(0) };
    static PEAK_BYTES: Cell<usize> = const { Cell::new(0) };
}

/// The shared capped allocator plus a per-thread high-water mark.
///
/// `smoke::CappedAllocator` only notices a parser that takes more than 1 GiB.
/// A reader that sizes a 256 MiB buffer from a header field of a 300-byte file
/// is the same defect at a smaller scale, so the tests here also assert how
/// much memory one parse call held at its peak.
struct TrackingAllocator;

impl TrackingAllocator {
    fn charge(size: usize) {
        // `try_with` so an allocation made during thread teardown is skipped
        // instead of panicking inside the allocator.
        let _ = LIVE_BYTES.try_with(|live| {
            let now = live.get().saturating_add(size);
            live.set(now);
            let _ = PEAK_BYTES.try_with(|peak| peak.set(peak.get().max(now)));
        });
    }

    fn release(size: usize) {
        let _ = LIVE_BYTES.try_with(|live| live.set(live.get().saturating_sub(size)));
    }
}

// SAFETY: every method forwards the caller's unchanged pointer, layout and
// size to `smoke::CappedAllocator`, which forwards them to the system
// allocator. The only addition is thread-local byte accounting, which does
// not allocate.
unsafe impl GlobalAlloc for TrackingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        Self::charge(layout.size());
        // SAFETY: forwarding the unchanged layout.
        unsafe { smoke::CappedAllocator.alloc(layout) }
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        Self::charge(layout.size());
        // SAFETY: forwarding the unchanged layout.
        unsafe { smoke::CappedAllocator.alloc_zeroed(layout) }
    }

    unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
        Self::release(layout.size());
        // SAFETY: `pointer` came from this allocator with this layout.
        unsafe { smoke::CappedAllocator.dealloc(pointer, layout) }
    }

    unsafe fn realloc(&self, pointer: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        if new_size > layout.size() {
            Self::charge(new_size - layout.size());
        } else {
            Self::release(layout.size() - new_size);
        }
        // SAFETY: forwarding the original pointer and layout and the requested size.
        unsafe { smoke::CappedAllocator.realloc(pointer, layout, new_size) }
    }
}

#[global_allocator]
static ALLOCATOR: TrackingAllocator = TrackingAllocator;

/// Runs `parse` and returns its result with the most bytes this thread held
/// at once while it ran, counted from zero at the call.
fn peak_allocation<T>(parse: impl FnOnce() -> T) -> (T, usize) {
    LIVE_BYTES.with(|live| live.set(0));
    PEAK_BYTES.with(|peak| peak.set(0));
    let value = parse();
    (value, PEAK_BYTES.with(Cell::get))
}

/// Most memory one parse of a mutated seed may hold.
///
/// The largest expansion any reader here permits is the compressed-TIFF bound
/// of 32768 decoded bytes per input byte, and a mutated TIFF seed stays under
/// 1.5 KiB, so nothing legitimate reaches 64 MiB, while a buffer sized from a
/// header field (256 MiB under the `tiff` crate's default limit) is far over.
const MUTATION_ALLOCATION_BUDGET: usize = 64 << 20;

/// How long a mutation run may take before it is declared stuck.
///
/// The runs finish in a few seconds. This is a hang detector, not a budget:
/// iteration counts are fixed, and a slow machine only has to beat two minutes.
const MUTATION_DEADLINE: Duration = Duration::from_secs(120);

/// How long a test of a few crafted inputs may take before it is declared
/// stuck. Each of them finishes in well under a second.
const SINGLE_INPUT_DEADLINE: Duration = Duration::from_secs(15);

/// Serializes the tests in this binary.
///
/// The mutation harness swaps the process-wide panic hook, so two runs at once
/// would misreport each other's panic locations and silence the messages of
/// unrelated tests.
static SERIAL: Mutex<()> = Mutex::new(());

/// Runs `body` on its own thread, alone, and aborts if it does not finish.
///
/// A parser that loops without consuming input and without allocating trips
/// neither the allocation cap nor a panic; without a deadline it would hang
/// the test run. The stuck thread cannot be cancelled, so the process aborts
/// with a message, as the allocation cap does.
fn guarded<F>(what: &'static str, deadline: Duration, body: F)
where
    F: FnOnce() + Send + 'static,
{
    let _serial = SERIAL.lock().unwrap_or_else(PoisonError::into_inner);
    // The allocation cap reports through `eprintln!`. The first lock of stderr
    // in a process allocates its mutex, which would re-enter the allocator
    // that is in the middle of reporting; taking the lock once here, before
    // any parser runs, leaves nothing for that report to allocate.
    drop(std::io::stderr().lock());
    let (done, finished) = mpsc::channel();
    let worker = std::thread::Builder::new()
        .name(what.to_string())
        .spawn(move || {
            body();
            let _ = done.send(());
        })
        .expect("spawn test worker thread");
    match finished.recv_timeout(deadline) {
        Ok(()) | Err(RecvTimeoutError::Disconnected) => {
            if let Err(panic) = worker.join() {
                std::panic::resume_unwind(panic);
            }
        }
        Err(RecvTimeoutError::Timeout) => {
            // Written to the stream itself: the test harness captures
            // `eprintln!`, and captured output is lost when the process aborts.
            let _ = writeln!(
                std::io::stderr(),
                "malformed_input_smoke: `{what}` did not finish within {deadline:?}; \
                 a parser looped without consuming input"
            );
            std::process::abort();
        }
    }
}

/// Runs the mutation harness with a per-input allocation budget.
fn mutation_run<F>(name: &'static str, seeds: Vec<Vec<u8>>, iterations: usize, parse: F)
where
    F: Fn(&[u8]) + Send + 'static,
{
    guarded(name, MUTATION_DEADLINE, move || {
        smoke::assert_no_panics(name, &seeds, iterations, |bytes| {
            let ((), peak) = peak_allocation(|| parse(bytes));
            assert!(
                peak <= MUTATION_ALLOCATION_BUDGET,
                "{name}: a {} byte input made the parser hold {peak} bytes",
                bytes.len()
            );
        });
    });
}

/// Calls every geometry helper a consumer runs on parsed features.
///
/// The parsers hand coordinates straight from the file to these helpers, so a
/// hostile coordinate must be as harmless here as in the parser itself.
fn exercise_features(features: &[GeoFeature]) {
    let mut boxes = Vec::new();
    for (index, feature) in features.iter().enumerate() {
        let geometry = &feature.geometry;
        let _ = geometry.to_wkt();
        let _ = geometry.representative_point();
        let _ = geometry.is_empty();
        let _ = geometry.coordinate_count();
        let _ = geometry.crosses_antimeridian();
        let _ = geometry_area_m2(geometry);
        for part in geometry.split_antimeridian() {
            let _ = part.bbox();
        }
        if let Some(bbox) = feature.bbox() {
            boxes.push((bbox, index));
        }
    }
    let _ = features_to_geojson(features);
    let index = SpatialIndex::bulk_load(boxes);
    let _ = index.query_antimeridian_aware(&BBox::new(170.0, -10.0, -170.0, 10.0));
}

fn push_f64s(out: &mut Vec<u8>, values: &[f64]) {
    for value in values {
        out.extend_from_slice(&value.to_le_bytes());
    }
}

// ── Shapefile seeds ──────────────────────────────────────────────────────────

const SHP_NULL: i32 = 0;
const SHP_POINT: i32 = 1;
const SHP_POLYLINE: i32 = 3;
const SHP_POLYGON: i32 = 5;
const SHP_MULTIPOINT: i32 = 8;
const SHP_POINT_Z: i32 = 11;
const SHP_POLYGON_Z: i32 = 15;
const SHP_MULTIPOINT_Z: i32 = 18;
const SHP_POINT_M: i32 = 21;
const SHP_POLYLINE_M: i32 = 23;
const SHP_MULTIPOINT_M: i32 = 28;

/// The 100-byte `.shp` file header for a file with `body_len` record bytes.
fn shp_header(body_len: usize) -> Vec<u8> {
    let mut header = Vec::with_capacity(100);
    header.extend_from_slice(&9994i32.to_be_bytes());
    header.extend_from_slice(&[0u8; 20]);
    header.extend_from_slice(&(((100 + body_len) / 2) as i32).to_be_bytes());
    header.extend_from_slice(&1000i32.to_le_bytes());
    header.extend_from_slice(&SHP_POLYGON.to_le_bytes());
    push_f64s(
        &mut header,
        &[-180.0, -90.0, 180.0, 90.0, 0.0, 0.0, 0.0, 0.0],
    );
    assert_eq!(header.len(), 100);
    header
}

/// A `.shp` file whose records carry the given contents, numbered from 1.
fn shp_file(contents: &[Vec<u8>]) -> Vec<u8> {
    let mut body = Vec::new();
    for (index, content) in contents.iter().enumerate() {
        body.extend_from_slice(&(index as i32 + 1).to_be_bytes());
        body.extend_from_slice(&((content.len() / 2) as i32).to_be_bytes());
        body.extend_from_slice(content);
    }
    let mut file = shp_header(body.len());
    file.extend_from_slice(&body);
    file
}

/// A `.shp` file with one record whose header claims `length_words` of content.
fn shp_file_with_record_length(length_words: i32, content: &[u8]) -> Vec<u8> {
    let mut file = shp_header(8 + content.len());
    file.extend_from_slice(&1i32.to_be_bytes());
    file.extend_from_slice(&length_words.to_be_bytes());
    file.extend_from_slice(content);
    file
}

fn shp_null() -> Vec<u8> {
    SHP_NULL.to_le_bytes().to_vec()
}

/// A Point, PointZ or PointM record: `extra` holds Z and/or M.
fn shp_point(shape_type: i32, x: f64, y: f64, extra: &[f64]) -> Vec<u8> {
    let mut content = shape_type.to_le_bytes().to_vec();
    push_f64s(&mut content, &[x, y]);
    push_f64s(&mut content, extra);
    content
}

/// A `[min, max, values…]` Z or M block.
fn push_measures(out: &mut Vec<u8>, values: &[f64]) {
    let min = values.iter().copied().fold(f64::INFINITY, f64::min);
    let max = values.iter().copied().fold(f64::NEG_INFINITY, f64::max);
    push_f64s(out, &[min, max]);
    push_f64s(out, values);
}

fn shp_multipoint(
    shape_type: i32,
    points: &[[f64; 2]],
    z: Option<&[f64]>,
    m: Option<&[f64]>,
) -> Vec<u8> {
    let mut content = shape_type.to_le_bytes().to_vec();
    push_f64s(&mut content, &[-180.0, -90.0, 180.0, 90.0]);
    content.extend_from_slice(&(points.len() as i32).to_le_bytes());
    for point in points {
        push_f64s(&mut content, point);
    }
    for values in [z, m].into_iter().flatten() {
        push_measures(&mut content, values);
    }
    content
}

/// A PolyLine or Polygon record (plain, Z or M) from its parts.
fn shp_poly(
    shape_type: i32,
    parts: &[&[[f64; 2]]],
    z: Option<&[f64]>,
    m: Option<&[f64]>,
) -> Vec<u8> {
    let mut starts = Vec::new();
    let mut points = Vec::new();
    for part in parts {
        starts.push(points.len() as i32);
        points.extend_from_slice(part);
    }
    let mut content = shp_poly_raw(shape_type, &starts, points.len() as i32, &points);
    for values in [z, m].into_iter().flatten() {
        push_measures(&mut content, values);
    }
    content
}

/// A PolyLine or Polygon record with the part table and point count as given,
/// whether or not they agree with `points`.
fn shp_poly_raw(
    shape_type: i32,
    part_starts: &[i32],
    declared_points: i32,
    points: &[[f64; 2]],
) -> Vec<u8> {
    shp_poly_declaring(
        shape_type,
        part_starts.len() as i32,
        declared_points,
        part_starts,
        points,
    )
}

/// As [`shp_poly_raw`], with the part count declared independently too.
fn shp_poly_declaring(
    shape_type: i32,
    declared_parts: i32,
    declared_points: i32,
    part_starts: &[i32],
    points: &[[f64; 2]],
) -> Vec<u8> {
    let mut content = shape_type.to_le_bytes().to_vec();
    push_f64s(&mut content, &[-180.0, -90.0, 180.0, 90.0]);
    content.extend_from_slice(&declared_parts.to_le_bytes());
    content.extend_from_slice(&declared_points.to_le_bytes());
    for start in part_starts {
        content.extend_from_slice(&start.to_le_bytes());
    }
    for point in points {
        push_f64s(&mut content, point);
    }
    content
}

const SQUARE: [[f64; 2]; 5] = [
    [-48.0, -16.0],
    [-47.0, -16.0],
    [-47.0, -15.0],
    [-48.0, -15.0],
    [-48.0, -16.0],
];
const HOLE: [[f64; 2]; 4] = [
    [-47.6, -15.6],
    [-47.4, -15.6],
    [-47.4, -15.4],
    [-47.6, -15.6],
];
const LINE: [[f64; 2]; 3] = [[-47.0, -15.0], [-46.5, -14.5], [-46.0, -14.0]];
const DATELINE_LINE: [[f64; 2]; 3] = [[179.0, 10.0], [-179.0, 11.0], [-178.0, 12.0]];

/// One record of each plain shape type: Point, PolyLine, Polygon, MultiPoint.
fn shp_plain_seed() -> Vec<u8> {
    shp_file(&[
        shp_point(SHP_POINT, -47.9172, -15.7797, &[]),
        shp_poly(SHP_POLYLINE, &[&LINE, &DATELINE_LINE], None, None),
        shp_poly(SHP_POLYGON, &[&SQUARE, &HOLE], None, None),
        shp_multipoint(SHP_MULTIPOINT, &LINE, None, None),
    ])
}

/// Z and M shape types, and a null shape.
fn shp_measured_seed() -> Vec<u8> {
    shp_file(&[
        shp_point(SHP_POINT_Z, -47.9, -15.8, &[250.5, 1.0]),
        shp_point(SHP_POINT_M, -47.9, -15.8, &[7.0]),
        shp_null(),
        shp_multipoint(
            SHP_MULTIPOINT_Z,
            &LINE,
            Some(&[1.0, 2.0, 3.0]),
            Some(&[4.0, 5.0, 6.0]),
        ),
        shp_multipoint(SHP_MULTIPOINT_M, &LINE, None, Some(&[4.0, 5.0, 6.0])),
        shp_poly(
            SHP_POLYGON_Z,
            &[&SQUARE],
            Some(&[1.0, 2.0, 3.0, 4.0, 1.0]),
            Some(&[0.0, 0.5, 1.0, 1.5, 2.0]),
        ),
        shp_poly(SHP_POLYLINE_M, &[&LINE], None, Some(&[0.0, 0.5, 1.0])),
    ])
}

/// One dBASE field: name, type character, width, decimal count.
type DbfField = (&'static str, u8, u8, u8);

/// A dBASE III table. Each row is its deletion flag and one value per field.
fn dbf_file(fields: &[DbfField], rows: &[(bool, Vec<&str>)]) -> Vec<u8> {
    let record_size: usize = 1 + fields.iter().map(|field| field.2 as usize).sum::<usize>();
    let header_size = 32 + 32 * fields.len() + 1;
    let mut file = vec![0x03, 126, 11, 12];
    file.extend_from_slice(&(rows.len() as u32).to_le_bytes());
    file.extend_from_slice(&(header_size as u16).to_le_bytes());
    file.extend_from_slice(&(record_size as u16).to_le_bytes());
    file.extend_from_slice(&[0u8; 20]);
    for (name, field_type, width, decimals) in fields {
        let mut descriptor = [0u8; 32];
        descriptor[..name.len()].copy_from_slice(name.as_bytes());
        descriptor[11] = *field_type;
        descriptor[16] = *width;
        descriptor[17] = *decimals;
        file.extend_from_slice(&descriptor);
    }
    file.push(0x0D);
    for (deleted, values) in rows {
        file.push(if *deleted { b'*' } else { b' ' });
        for ((_, _, width, _), value) in fields.iter().zip(values) {
            let mut cell = value.as_bytes().to_vec();
            cell.resize(*width as usize, b' ');
            file.extend_from_slice(&cell);
        }
    }
    file.push(0x1A);
    file
}

const DBF_FIELDS: [DbfField; 3] = [
    ("NAME", b'C', 12, 0),
    ("AREA_KM2", b'N', 10, 2),
    ("SURVEYED", b'D', 8, 0),
];

/// Character, numeric and date fields; four rows to match [`shp_plain_seed`].
fn dbf_seed() -> Vec<u8> {
    dbf_file(
        &DBF_FIELDS,
        &[
            (false, vec!["Brasilia", "   5802.00", "20240115"]),
            (false, vec!["Rio Preto", "    431.50", "20231201"]),
            (false, vec!["Lago Sul", "    183.39", "20220630"]),
            (false, vec!["Estacoes", "      0.00", "20210101"]),
        ],
    )
}

/// As [`dbf_seed`], with a deleted row in the middle.
fn dbf_seed_with_deleted_row() -> Vec<u8> {
    dbf_file(
        &DBF_FIELDS,
        &[
            (false, vec!["Brasilia", "   5802.00", "20240115"]),
            (true, vec!["Removed", "      1.00", "19990101"]),
            (false, vec!["Rio Preto", "    431.50", "20231201"]),
            (false, vec!["Lago Sul", "    183.39", "20220630"]),
            (false, vec!["Estacoes", "      0.00", "20210101"]),
        ],
    )
}

/// The fixed part of a dBASE header: record count, header size, record size.
fn dbf_header(num_records: u32, header_size: u16, record_size: u16) -> Vec<u8> {
    let mut header = vec![0x03, 126, 11, 12];
    header.extend_from_slice(&num_records.to_le_bytes());
    header.extend_from_slice(&header_size.to_le_bytes());
    header.extend_from_slice(&record_size.to_le_bytes());
    header.extend_from_slice(&[0u8; 20]);
    header
}

// ── TIFF seeds ───────────────────────────────────────────────────────────────

const TIFF_ASCII: u16 = 2;
const TIFF_SHORT: u16 = 3;
const TIFF_LONG: u16 = 4;
const TIFF_DOUBLE: u16 = 12;

const TAG_IMAGE_WIDTH: u16 = 256;
const TAG_IMAGE_LENGTH: u16 = 257;
const TAG_BITS_PER_SAMPLE: u16 = 258;
const TAG_COMPRESSION: u16 = 259;
const TAG_PHOTOMETRIC: u16 = 262;
const TAG_STRIP_OFFSETS: u16 = 273;
const TAG_SAMPLES_PER_PIXEL: u16 = 277;
const TAG_ROWS_PER_STRIP: u16 = 278;
const TAG_STRIP_BYTE_COUNTS: u16 = 279;
const TAG_SAMPLE_FORMAT: u16 = 339;
const TAG_MODEL_PIXEL_SCALE: u16 = 33550;
const TAG_MODEL_TIEPOINT: u16 = 33922;
const TAG_MODEL_TRANSFORMATION: u16 = 34264;
const TAG_GEO_KEY_DIRECTORY: u16 = 34735;
const TAG_GEO_DOUBLE_PARAMS: u16 = 34736;
const TAG_GEO_ASCII_PARAMS: u16 = 34737;

/// A little-endian, single-strip, single-directory TIFF under construction.
#[derive(Clone)]
struct Tiff {
    /// Tag, field type, value count, value bytes.
    entries: Vec<(u16, u16, u32, Vec<u8>)>,
}

impl Tiff {
    fn new() -> Self {
        Self {
            entries: Vec::new(),
        }
    }

    /// A baseline image header: size, sample layout, compression.
    fn image(width: u32, height: u32, bits: &[u16], compression: u16) -> Self {
        let photometric = if bits.len() >= 3 { 2 } else { 1 };
        Self::new()
            .longs(TAG_IMAGE_WIDTH, &[width])
            .longs(TAG_IMAGE_LENGTH, &[height])
            .shorts(TAG_BITS_PER_SAMPLE, bits)
            .shorts(TAG_COMPRESSION, &[compression])
            .shorts(TAG_PHOTOMETRIC, &[photometric])
            .shorts(TAG_SAMPLES_PER_PIXEL, &[bits.len() as u16])
            .longs(TAG_ROWS_PER_STRIP, &[height])
    }

    /// Georeferencing: pixel scale, one tiepoint, a GeoKey directory and its
    /// double and ASCII parameter tags.
    fn georeferenced(self) -> Self {
        self.doubles(TAG_MODEL_PIXEL_SCALE, &[0.25, 0.25, 0.0])
            .doubles(TAG_MODEL_TIEPOINT, &[0.0, 0.0, 0.0, -48.0, -15.0, 0.0])
            .shorts(
                TAG_GEO_KEY_DIRECTORY,
                &[
                    1, 1, 0, 4, // version 1.1.0, four keys
                    1024, 0, 1, 2, // GTModelType = geographic
                    1025, 0, 1, 1, // GTRasterType = pixel is area
                    2048, 0, 1, 4326, // GeographicType = WGS 84
                    2049, 34737, 7, 0, // GeogCitation -> ASCII params
                ],
            )
            .doubles(TAG_GEO_DOUBLE_PARAMS, &[6_378_137.0, 298.257_223_563])
            .ascii(TAG_GEO_ASCII_PARAMS, "WGS 84|")
    }

    fn shorts(self, tag: u16, values: &[u16]) -> Self {
        let bytes = values.iter().flat_map(|v| v.to_le_bytes()).collect();
        self.raw(tag, TIFF_SHORT, values.len() as u32, bytes)
    }

    fn longs(self, tag: u16, values: &[u32]) -> Self {
        let bytes = values.iter().flat_map(|v| v.to_le_bytes()).collect();
        self.raw(tag, TIFF_LONG, values.len() as u32, bytes)
    }

    fn doubles(self, tag: u16, values: &[f64]) -> Self {
        let bytes = values.iter().flat_map(|v| v.to_le_bytes()).collect();
        self.raw(tag, TIFF_DOUBLE, values.len() as u32, bytes)
    }

    fn ascii(self, tag: u16, text: &str) -> Self {
        let mut bytes = text.as_bytes().to_vec();
        bytes.push(0);
        let count = bytes.len() as u32;
        self.raw(tag, TIFF_ASCII, count, bytes)
    }

    /// Adds or replaces an entry; `count` need not match `bytes`.
    fn raw(mut self, tag: u16, field_type: u16, count: u32, bytes: Vec<u8>) -> Self {
        self.entries.retain(|entry| entry.0 != tag);
        self.entries.push((tag, field_type, count, bytes));
        self
    }

    /// A classic TIFF: 32-bit offsets and counts.
    fn build(self, strip: &[u8]) -> Vec<u8> {
        self.layout(strip, false)
    }

    /// A BigTIFF: the same directory with 64-bit offsets and counts.
    fn build_big(self, strip: &[u8]) -> Vec<u8> {
        self.layout(strip, true)
    }

    /// Lays the file out: header, directory, out-of-line values, then `strip`.
    fn layout(mut self, strip: &[u8], big: bool) -> Vec<u8> {
        // Offsets and counts are `word` bytes wide, and a value that fits in
        // one word is stored in the entry itself.
        let word = if big { 8 } else { 4 };
        let push_word = |file: &mut Vec<u8>, value: usize| {
            file.extend_from_slice(&(value as u64).to_le_bytes()[..word]);
        };

        let strip_len = (strip.len() as u32).to_le_bytes().to_vec();
        self = self.raw(TAG_STRIP_OFFSETS, TIFF_LONG, 1, vec![0; 4]).raw(
            TAG_STRIP_BYTE_COUNTS,
            TIFF_LONG,
            1,
            strip_len,
        );
        self.entries.sort_by_key(|entry| entry.0);

        let mut file = if big {
            b"II\x2b\x00\x08\x00\x00\x00\x10\x00\x00\x00\x00\x00\x00\x00".to_vec()
        } else {
            b"II\x2a\x00\x08\x00\x00\x00".to_vec()
        };
        let entry_count_len = if big { 8 } else { 2 };
        let values_start =
            file.len() + entry_count_len + (4 + 2 * word) * self.entries.len() + word;
        let values_len: usize = self
            .entries
            .iter()
            .filter(|entry| entry.3.len() > word)
            .map(|entry| entry.3.len() + entry.3.len() % 2)
            .sum();
        let strip_offset = (values_start + values_len) as u32;

        file.extend_from_slice(&(self.entries.len() as u64).to_le_bytes()[..entry_count_len]);
        let mut values = Vec::with_capacity(values_len);
        for (tag, field_type, count, mut bytes) in self.entries {
            if tag == TAG_STRIP_OFFSETS {
                bytes = strip_offset.to_le_bytes().to_vec();
            }
            file.extend_from_slice(&tag.to_le_bytes());
            file.extend_from_slice(&field_type.to_le_bytes());
            push_word(&mut file, count as usize);
            if bytes.len() <= word {
                bytes.resize(word, 0);
                file.extend_from_slice(&bytes);
            } else {
                push_word(&mut file, values_start + values.len());
                if bytes.len() % 2 == 1 {
                    bytes.push(0);
                }
                values.extend_from_slice(&bytes);
            }
        }
        push_word(&mut file, 0);
        file.extend_from_slice(&values);
        file.extend_from_slice(strip);
        file
    }
}

/// Sixteen gray levels for a 4×4 image.
fn gray_ramp() -> Vec<u8> {
    (0..16u8).map(|level| level * 16).collect()
}

/// A zlib stream holding `data` in one stored (uncompressed) block.
fn zlib_stored(data: &[u8]) -> Vec<u8> {
    let (mut a, mut b) = (1u32, 0u32);
    for byte in data {
        a = (a + u32::from(*byte)) % 65_521;
        b = (b + a) % 65_521;
    }
    let len = data.len() as u16;
    let mut stream = vec![0x78, 0x01, 0x01];
    stream.extend_from_slice(&len.to_le_bytes());
    stream.extend_from_slice(&(!len).to_le_bytes());
    stream.extend_from_slice(data);
    stream.extend_from_slice(&((b << 16) | a).to_be_bytes());
    stream
}

/// A 4×4 8-bit gray GeoTIFF, uncompressed, with a full GeoKey directory.
fn geotiff_gray8_seed() -> Vec<u8> {
    Tiff::image(4, 4, &[8], 1)
        .georeferenced()
        .build(&gray_ramp())
}

/// The same image as [`geotiff_gray8_seed`] in a BigTIFF container.
fn bigtiff_gray8_seed() -> Vec<u8> {
    Tiff::image(4, 4, &[8], 1)
        .georeferenced()
        .build_big(&gray_ramp())
}

/// A 4×3 16-bit gray TIFF, the layout thermal cameras write.
fn tiff_gray16_seed() -> Vec<u8> {
    let strip: Vec<u8> = (0..12u16)
        .flat_map(|pixel| (8000 + pixel * 500).to_le_bytes())
        .collect();
    Tiff::image(4, 3, &[16], 1).georeferenced().build(&strip)
}

/// A 2×2 RGB GeoTIFF with a full affine transformation instead of a tiepoint.
fn geotiff_rgb8_seed() -> Vec<u8> {
    let mut matrix = [0.0f64; 16];
    matrix[0] = 0.5;
    matrix[3] = -48.0;
    matrix[5] = -0.5;
    matrix[7] = -15.0;
    matrix[15] = 1.0;
    Tiff::image(2, 2, &[8, 8, 8], 1)
        .georeferenced()
        .doubles(TAG_MODEL_TRANSFORMATION, &matrix)
        .build(&[255, 0, 0, 0, 255, 0, 0, 0, 255, 128, 128, 128])
}

/// A 2×2 32-bit float GeoTIFF (an elevation or reflectance band).
fn geotiff_float32_seed() -> Vec<u8> {
    let strip: Vec<u8> = [1.5f32, -2.25, 1000.0, 0.0]
        .iter()
        .flat_map(|sample| sample.to_le_bytes())
        .collect();
    Tiff::image(2, 2, &[32], 1)
        .shorts(TAG_SAMPLE_FORMAT, &[3])
        .georeferenced()
        .build(&strip)
}

/// A 4×4 gray TIFF, PackBits-compressed (one literal run).
fn tiff_packbits_seed() -> Vec<u8> {
    let mut strip = vec![15u8];
    strip.extend_from_slice(&gray_ramp());
    Tiff::image(4, 4, &[8], 32773).georeferenced().build(&strip)
}

/// A 4×4 gray TIFF, Deflate-compressed.
fn tiff_deflate_seed() -> Vec<u8> {
    Tiff::image(4, 4, &[8], 8)
        .georeferenced()
        .build(&zlib_stored(&gray_ramp()))
}

/// Writes `bytes` to `file` and reads its raster bands back by path.
fn bands_from_bytes(
    file: &tempfile::NamedTempFile,
    bytes: &[u8],
) -> geo_fast_core::Result<Vec<Vec<f32>>> {
    std::fs::write(file.path(), bytes).expect("write temporary TIFF");
    parse_geotiff_bands(file.path())
}

// ── ENVI, FLIR, SAR and GeoJSON seeds ────────────────────────────────────────

/// An ENVI header for a `samples`×`lines`×`bands` cube, with a wavelength
/// table and geographic `map info`.
fn envi_header(
    samples: u32,
    lines: u32,
    bands: u32,
    data_type: u8,
    interleave: &str,
    byte_order: u8,
) -> Vec<u8> {
    envi_header_text(
        [&samples.to_string(), &lines.to_string(), &bands.to_string()],
        "0",
        &data_type.to_string(),
        interleave,
        &byte_order.to_string(),
    )
}

/// As [`envi_header`], with every numeric field given as text so a test can
/// write values no valid file holds. `dimensions` is samples, lines, bands.
fn envi_header_text(
    dimensions: [&str; 3],
    header_offset: &str,
    data_type: &str,
    interleave: &str,
    byte_order: &str,
) -> Vec<u8> {
    let [samples, lines, bands] = dimensions;
    format!(
        "ENVI\n\
         description = {{ Synthetic cube\n  for malformed-input tests }}\n\
         samples = {samples}\nlines = {lines}\nbands = {bands}\n\
         header offset = {header_offset}\nfile type = ENVI Standard\n\
         data type = {data_type}\ninterleave = {interleave}\nbyte order = {byte_order}\n\
         sensor type = AVIRIS\nwavelength units = Nanometers\n\
         map info = {{ Geographic Lat/Lon, 1, 1, -60.0, -10.0, 0.001, 0.001, WGS-84 }}\n\
         wavelength = {{ 450.0, 550.0,\n 650.0 }}\n\
         fwhm = {{ 10, 10, 10 }}\n"
    )
    .into_bytes()
}

/// Pixel data for a 4×3 cube of three bands of `element_size` bytes each.
fn envi_data(element_size: usize) -> Vec<u8> {
    (0..4 * 3 * 3 * element_size)
        .map(|index| (index * 7 % 251) as u8)
        .collect()
}

/// A FLIR radiometric JPEG: SOI, an APP1 segment holding an `FFF` record with
/// `width`×`height` raw 16-bit samples, EOI.
fn flir_rjpeg(width: u32, height: u32, samples: usize) -> Vec<u8> {
    let mut record = vec![0u8; 88];
    record[..4].copy_from_slice(b"FFF\0");
    record[4..8].copy_from_slice(&100u32.to_le_bytes());
    record[32..36].copy_from_slice(&88u32.to_le_bytes());
    record[36..40].copy_from_slice(&((samples * 2) as u32).to_le_bytes());
    record[40..44].copy_from_slice(&width.to_le_bytes());
    record[44..48].copy_from_slice(&height.to_le_bytes());
    let calibration = [
        21_106.77f32,
        0.012_545_258,
        1501.2,
        1.0,
        -7340.0,
        0.95,
        20.0,
        20.0,
        1.0,
    ];
    for (index, value) in calibration.iter().enumerate() {
        let at = 48 + index * 4;
        record[at..at + 4].copy_from_slice(&value.to_le_bytes());
    }
    for sample in 0..samples {
        record.extend_from_slice(&(9000 + (sample as u16) * 250).to_le_bytes());
    }

    let mut file = vec![0xFF, 0xD8, 0xFF, 0xE1];
    file.extend_from_slice(&((2 + 8 + record.len()) as u16).to_be_bytes());
    file.extend_from_slice(b"FLIR\0\x01\x00\x00");
    file.extend_from_slice(&record);
    file.extend_from_slice(&[0xFF, 0xD9]);
    file
}

/// A CEOS leader: a 720-byte file descriptor record, then a scene header
/// record with the fixed-width ASCII fields the reader extracts.
fn ceos_leader_seed() -> Vec<u8> {
    let mut descriptor = vec![b' '; 720];
    descriptor[..4].copy_from_slice(&1u32.to_be_bytes());
    descriptor[4..8].copy_from_slice(&[0x3F, 0xC0, 0x12, 0x12]);
    descriptor[8..12].copy_from_slice(&720u32.to_be_bytes());

    let mut scene = vec![b' '; 4096];
    let mut put = |offset: usize, text: &str| {
        scene[offset..offset + text.len()].copy_from_slice(text.as_bytes());
    };
    put(20, "ERS-1");
    put(68, "1994-07-21T13:45:10.123Z");
    put(100, "DESCENDING");
    put(116, "  -10.50");
    put(124, "  -60.25");
    put(132, "  -10.40");
    put(140, "  -59.10");
    put(148, "  -11.60");
    put(156, "  -59.00");
    put(164, "  -11.70");
    put(172, "  -60.15");
    put(228, "SLC");
    put(248, "    4912");
    put(256, "   26526");
    put(264, "         12.5000");
    put(484, "  23.000");

    let mut file = descriptor;
    file.extend_from_slice(&scene);
    file
}

const SAFE_MANIFEST: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<xfdu:XFDU xmlns:xfdu="urn:ccsds:schema:xfdu:1"
           xmlns:safe="http://www.esa.int/safe/sentinel-1.0"
           xmlns:s1sarl1="http://www.esa.int/safe/sentinel-1.0/sentinel-1/sar/level-1"
           xmlns:s1="http://www.esa.int/safe/sentinel-1.0/sentinel-1"
           xmlns:gml="http://www.opengis.net/gml">
  <metadataSection>
    <metadataObject ID="platform">
      <metadataWrap><xmlData>
        <safe:platform>
          <safe:familyName>SENTINEL-1</safe:familyName>
          <safe:number>A</safe:number>
        </safe:platform>
      </xmlData></metadataWrap>
    </metadataObject>
    <metadataObject ID="acquisitionPeriod">
      <metadataWrap><xmlData>
        <safe:acquisitionPeriod>
          <safe:startTime>2024-01-15T10:30:00.000Z</safe:startTime>
        </safe:acquisitionPeriod>
      </xmlData></metadataWrap>
    </metadataObject>
    <metadataObject ID="generalProductInformation">
      <metadataWrap><xmlData>
        <s1sarl1:productType>GRD</s1sarl1:productType>
        <s1sarl1:transmitterReceiverPolarisation>VV</s1sarl1:transmitterReceiverPolarisation>
        <s1sarl1:transmitterReceiverPolarisation>VH</s1sarl1:transmitterReceiverPolarisation>
        <s1:pass>ASCENDING</s1:pass>
        <s1sarl1:resolution>10.0</s1sarl1:resolution>
        <s1sarl1:numberOfSamples>25000</s1sarl1:numberOfSamples>
        <s1sarl1:numberOfLines>16000</s1sarl1:numberOfLines>
        <s1sarl1:incidenceAngleMidSwath>38.5 &amp; rising</s1sarl1:incidenceAngleMidSwath>
        <gml:coordinates>-10.0,-60.0 -10.0,-58.0 -12.0,-58.0 -12.0,-60.0</gml:coordinates>
        <gml:posList>-10.0 -60.0 -10.0 -58.0 -12.0 -58.0</gml:posList>
      </xmlData></metadataWrap>
    </metadataObject>
  </metadataSection>
</xfdu:XFDU>"#;

const SAR_ANNOTATION: &str = r#"<?xml version="1.0"?>
<product>
  <geolocationGrid>
    <geolocationGridPointList count="2">
      <geolocationGridPoint>
        <line>0</line><pixel>0</pixel>
        <latitude>-10.5</latitude><longitude>-59.2</longitude><height>150.0</height>
      </geolocationGridPoint>
      <geolocationGridPoint>
        <line>100</line><pixel>200</pixel>
        <latitude>-11.0</latitude><longitude>-58.8</longitude><height>120.0</height>
      </geolocationGridPoint>
    </geolocationGridPointList>
  </geolocationGrid>
</product>"#;

/// A FeatureCollection with every geometry type the reader accepts.
const GEOJSON_SEED: &str = r#"{
  "type": "FeatureCollection",
  "bbox": [-180.0, -90.0, 180.0, 90.0],
  "features": [
    {"type": "Feature", "id": "p1",
     "geometry": {"type": "Point", "coordinates": [-47.9172, -15.7797, 1172.0]},
     "properties": {"name": "Brasília", "population": 2817000, "tags": ["capital", null]}},
    {"type": "Feature", "id": 2,
     "geometry": {"type": "MultiPoint", "coordinates": [[-47.0, -15.0], [179.5, 10.0]]},
     "properties": {}},
    {"type": "Feature", "id": null,
     "geometry": {"type": "LineString", "coordinates": [[179.0, 10.0], [-179.0, 11.0], [-178.0, 12.0]]},
     "properties": {"nested": {"a": {"b": [1, 2.5e3, true]}}}},
    {"type": "Feature",
     "geometry": {"type": "MultiLineString",
       "coordinates": [[[-47.0, -15.0], [-46.0, -14.0]], [[10.0, 10.0], [11.0, 11.0], [12.0, 10.0]]]},
     "properties": null},
    {"type": "Feature",
     "geometry": {"type": "Polygon",
       "coordinates": [[[-48.0, -16.0], [-47.0, -16.0], [-47.0, -15.0], [-48.0, -15.0], [-48.0, -16.0]],
                       [[-47.6, -15.6], [-47.4, -15.6], [-47.4, -15.4], [-47.6, -15.6]]]},
     "properties": {"area": 1.0e4}},
    {"type": "Feature",
     "geometry": {"type": "MultiPolygon",
       "coordinates": [[[[0.0, 0.0], [1.0, 0.0], [1.0, 1.0], [0.0, 0.0]]],
                       [[[170.0, -10.0], [-170.0, -10.0], [-170.0, 10.0], [170.0, 10.0], [170.0, -10.0]]]]},
     "properties": {"name": "two"}}
  ]
}"#;

/// A bare Feature, and the one standard geometry type the reader rejects.
const GEOJSON_COLLECTION_SEED: &str = r#"{"type": "Feature", "id": 7,
  "geometry": {"type": "GeometryCollection", "geometries": [
    {"type": "Point", "coordinates": [1.0, 2.0]},
    {"type": "GeometryCollection", "geometries": [
      {"type": "LineString", "coordinates": [[0.0, 0.0], [1.0, 1.0]]}]}]},
  "properties": {"kind": "collection"}}"#;

// ── The seeds are what they claim to be ──────────────────────────────────────

#[test]
fn seeds_are_valid_files() {
    guarded("seeds", SINGLE_INPUT_DEADLINE, || {
        let plain = parse_shapefile_bytes(&shp_plain_seed(), Some(&dbf_seed())).expect("plain shp");
        assert_eq!(plain.features.len(), 4);
        assert_eq!(plain.schema.len(), 3);
        assert!(matches!(plain.features[0].geometry, Geometry::Point(_)));
        assert!(
            matches!(&plain.features[1].geometry, Geometry::Polyline(parts) if parts.len() == 2)
        );
        assert!(
            matches!(&plain.features[2].geometry, Geometry::Polygon(rings) if rings.len() == 2)
        );
        assert!(
            matches!(&plain.features[3].geometry, Geometry::MultiPoint(points) if points.len() == 3)
        );
        assert_eq!(
            plain.features[1]
                .get_property("NAME")
                .and_then(|v| v.as_str()),
            Some("Rio Preto")
        );
        assert_eq!(
            plain.features[2]
                .get_property("AREA_KM2")
                .and_then(|v| v.as_str()),
            Some("183.39")
        );
        assert_eq!(
            plain.features[3]
                .get_property("SURVEYED")
                .and_then(|v| v.as_str()),
            Some("20210101")
        );
        exercise_features(&plain.features);

        let with_deleted =
            parse_shapefile_bytes(&shp_plain_seed(), Some(&dbf_seed_with_deleted_row()))
                .expect("shp with a deleted dbf row");
        assert_eq!(with_deleted.features.len(), 4);
        assert_eq!(
            with_deleted.features[1]
                .get_property("NAME")
                .and_then(|v| v.as_str()),
            Some("Rio Preto")
        );

        let measured = parse_shapefile_bytes(&shp_measured_seed(), None).expect("measured shp");
        assert_eq!(measured.features.len(), 6, "the null shape is skipped");
        assert_eq!(
            measured.features[0]
                .get_property("_z")
                .and_then(|v| v.as_f64()),
            Some(250.5)
        );
        assert_eq!(
            measured.features[4]
                .get_property("_m")
                .and_then(|v| v.as_array())
                .map(Vec::len),
            Some(5)
        );
        assert_eq!(
            measured.features[5]
                .get_property("_m")
                .and_then(|v| v.as_array())
                .map(Vec::len),
            Some(3)
        );

        let metadata = parse_geotiff_metadata_reader(Cursor::new(geotiff_gray8_seed()))
            .expect("gray GeoTIFF metadata");
        assert_eq!(
            (metadata.width, metadata.height, metadata.band_count),
            (4, 4, 1)
        );
        assert_eq!(metadata.epsg_code, Some(4326));
        assert!(metadata.is_geographic, "GTModelType 2 is geographic");
        assert_eq!(metadata.pixel_scale, Some((0.25, 0.25)));
        assert_eq!(metadata.tiepoints.len(), 1);
        assert_eq!(metadata.geokeys.len(), 4);
        let bbox = metadata.bbox.expect("bbox from tiepoint and scale");
        assert_eq!(bbox, BBox::new(-48.0, -16.0, -47.0, -15.0));

        let rgb = parse_geotiff_metadata_reader(Cursor::new(geotiff_rgb8_seed()))
            .expect("RGB GeoTIFF metadata");
        assert_eq!(rgb.band_count, 3);
        assert!(rgb.transformation_matrix.is_some());

        let big = parse_geotiff_metadata_reader(Cursor::new(bigtiff_gray8_seed()))
            .expect("BigTIFF metadata");
        assert_eq!((big.width, big.height, big.epsg_code), (4, 4, Some(4326)));
        assert_eq!(big.bbox, metadata.bbox);

        let scratch = tempfile::NamedTempFile::new().expect("temporary file");
        for (seed, bands, pixels) in [
            (geotiff_gray8_seed(), 1, 16),
            (bigtiff_gray8_seed(), 1, 16),
            (tiff_gray16_seed(), 1, 12),
            (geotiff_rgb8_seed(), 3, 4),
            (geotiff_float32_seed(), 1, 4),
            (tiff_packbits_seed(), 1, 16),
            (tiff_deflate_seed(), 1, 16),
        ] {
            let decoded = bands_from_bytes(&scratch, &seed).expect("raster bands");
            assert_eq!(decoded.len(), bands);
            assert!(decoded.iter().all(|band| band.len() == pixels));
        }

        let (thermal, temperatures) =
            ThermalParser::parse_thermal_tiff(&tiff_gray16_seed()).expect("thermal TIFF");
        assert_eq!((thermal.width, thermal.height), (4, 3));
        assert_eq!(temperatures.len(), 12);

        let (flir, temperatures) =
            ThermalParser::parse_flir_rjpeg(&flir_rjpeg(4, 3, 12)).expect("FLIR R-JPEG");
        assert_eq!((flir.width, flir.height), (4, 3));
        assert_eq!(temperatures.len(), 12);
        assert!((flir.emissivity - 0.95).abs() < 1e-6);

        for (data_type, element_size) in [(1u8, 1usize), (2, 2), (4, 4), (5, 8), (12, 2), (15, 8)] {
            for interleave in ["bsq", "bil", "bip"] {
                let header = envi_header(4, 3, 3, data_type, interleave, 1);
                let cube = HyperspectralParser::parse_envi_header(&header).expect("ENVI header");
                assert_eq!((cube.samples, cube.lines, cube.bands), (4, 3, 3));
                assert_eq!(cube.wavelengths.len(), 3);
                assert!(cube.bbox.is_some());
                let data = envi_data(element_size);
                for band in 0..3 {
                    let pixels =
                        HyperspectralParser::read_band(&data, &cube, band).expect("ENVI band");
                    assert_eq!(pixels.len(), 12);
                }
            }
        }

        let manifest =
            SarParser::parse_safe_manifest(SAFE_MANIFEST.as_bytes()).expect("SAFE manifest");
        assert_eq!(manifest.sensor, "SENTINEL-1A");
        assert_eq!(manifest.polarization, vec!["VV", "VH"]);
        assert_eq!((manifest.width, manifest.height), (25_000, 16_000));
        let points = SarParser::extract_gcps(SAR_ANNOTATION.as_bytes()).expect("annotation");
        assert_eq!(points.len(), 2);
        let leader = SarParser::parse_ceos_leader(&ceos_leader_seed()).expect("CEOS leader");
        assert_eq!(leader.sensor, "ERS-1");
        assert_eq!((leader.width, leader.height), (4912, 26_526));
        assert_eq!(leader.bbox, BBox::new(-60.25, -11.7, -59.0, -10.4));

        let features = parse_geojson_str(GEOJSON_SEED).expect("GeoJSON seed");
        assert_eq!(features.len(), 6);
        assert!(
            matches!(&features[5].geometry, Geometry::MultiPolygon(polygons) if polygons.len() == 2)
        );
        exercise_features(&features);
        assert!(parse_geojson_str(GEOJSON_COLLECTION_SEED).is_err());
    });
}

// ── Shapefile regressions ────────────────────────────────────────────────────

/// A 128-byte `.dbf` whose header claims four billion records asked for
/// tens of gigabytes up front.
#[test]
fn dbf_record_count_beyond_the_file_is_rejected() {
    guarded("dbf record count", SINGLE_INPUT_DEADLINE, || {
        for declared in [u32::MAX, 250_000_000, 1_000] {
            let mut dbf = dbf_header(declared, 32 + 32 + 1, 9);
            let mut descriptor = [0u8; 32];
            descriptor[..4].copy_from_slice(b"NAME");
            descriptor[11] = b'C';
            descriptor[16] = 8;
            dbf.extend_from_slice(&descriptor);
            dbf.push(0x0D);
            dbf.extend_from_slice(b" only one");
            dbf.resize(128, 0);

            let (result, peak) =
                peak_allocation(|| parse_shapefile_bytes(&shp_plain_seed(), Some(&dbf)));
            assert!(
                result.is_err(),
                "{declared} records do not fit in 128 bytes"
            );
            assert!(peak < 64 << 10, "held {peak} bytes for a 128 byte table");
        }
    });
}

/// `header_size` is a 16-bit field; the descriptor table it implies must fit.
#[test]
fn dbf_header_size_beyond_the_file_is_rejected() {
    guarded("dbf header size", SINGLE_INPUT_DEADLINE, || {
        let mut dbf = dbf_header(1, u16::MAX, 9);
        dbf.extend_from_slice(&[b'A'; 40]);
        let (result, peak) =
            peak_allocation(|| parse_shapefile_bytes(&shp_plain_seed(), Some(&dbf)));
        assert!(result.is_err());
        assert!(peak < 16 << 10, "held {peak} bytes for a 72 byte table");
    });
}

/// A deleted row is skipped by `record_size - 1` bytes; a zero record size
/// underflowed that subtraction.
#[test]
fn dbf_zero_record_size_is_rejected() {
    guarded("dbf zero record size", SINGLE_INPUT_DEADLINE, || {
        let mut dbf = dbf_seed_with_deleted_row();
        dbf[10..12].copy_from_slice(&0u16.to_le_bytes());
        assert!(parse_shapefile_bytes(&shp_plain_seed(), Some(&dbf)).is_err());

        // No fields at all: every row is the deletion flag alone.
        let mut flags_only = dbf_header(64, 33, 0);
        flags_only.push(0x0D);
        flags_only.extend_from_slice(&[b'*'; 64]);
        assert!(parse_shapefile_bytes(&shp_plain_seed(), Some(&flags_only)).is_err());
    });
}

/// Two thousand zero-width fields made every one-byte record allocate a
/// two-thousand-slot row: 48 KiB of row for each byte of record, so 25 KiB of
/// records was enough to pass a gigabyte.
#[test]
fn dbf_zero_width_fields_cannot_amplify() {
    guarded("dbf zero-width fields", SINGLE_INPUT_DEADLINE, || {
        const FIELDS: usize = 2046;
        const ROWS: usize = 2_000;
        let mut dbf = dbf_header(ROWS as u32, (32 + 32 * FIELDS + 1) as u16, 1);
        for index in 0..FIELDS {
            let mut descriptor = [0u8; 32];
            let name = format!("F{index}");
            descriptor[..name.len()].copy_from_slice(name.as_bytes());
            descriptor[11] = b'C';
            dbf.extend_from_slice(&descriptor);
        }
        dbf.push(0x0D);
        dbf.extend_from_slice(&vec![b' '; ROWS]);

        let (result, peak) =
            peak_allocation(|| parse_shapefile_bytes(&shp_plain_seed(), Some(&dbf)));
        assert!(result.is_err(), "a zero-width field is not a dBASE field");
        assert!(
            peak <= 64 * dbf.len(),
            "held {peak} bytes for a {} byte table",
            dbf.len()
        );
    });
}

/// A negative record length overflowed the length arithmetic; in a release
/// build it moved the cursor backwards, so the same record was read forever.
#[test]
fn shp_negative_record_length_is_rejected() {
    guarded("shp negative record length", SINGLE_INPUT_DEADLINE, || {
        let point = shp_point(SHP_POINT, 1.0, 2.0, &[]);
        for length_words in [-1, -4, -6, i32::MIN] {
            let shp = shp_file_with_record_length(length_words, &point);
            let (result, peak) = peak_allocation(|| parse_shapefile_bytes(&shp, None));
            assert!(result.is_err(), "record length {length_words}");
            assert!(peak < 64 << 10, "held {peak} bytes for a 128 byte file");
        }
    });
}

/// A record longer than the rest of the file is truncated input.
#[test]
fn shp_record_length_beyond_the_file_is_rejected() {
    guarded("shp record length", SINGLE_INPUT_DEADLINE, || {
        let point = shp_point(SHP_POINT, 1.0, 2.0, &[]);
        for length_words in [11, 1 << 20, i32::MAX] {
            let shp = shp_file_with_record_length(length_words, &point);
            assert!(
                parse_shapefile_bytes(&shp, None).is_err(),
                "record length {length_words}"
            );
        }
    });
}

/// Point and part counts come from the record; they sized allocations before
/// a single coordinate was read.
#[test]
fn shp_counts_beyond_the_record_are_rejected() {
    guarded("shp counts", SINGLE_INPUT_DEADLINE, || {
        let mut cases: Vec<(String, Vec<u8>)> = Vec::new();
        for declared in [-1, i32::MIN, i32::MAX, 0x0400_0000, 4] {
            for shape_type in [SHP_MULTIPOINT, SHP_MULTIPOINT_Z, SHP_MULTIPOINT_M] {
                let mut content = shp_multipoint(shape_type, &LINE, None, None);
                content[36..40].copy_from_slice(&declared.to_le_bytes());
                cases.push((
                    format!("multipoint {shape_type} with {declared} points"),
                    content,
                ));
            }
            for shape_type in [SHP_POLYLINE, SHP_POLYGON, SHP_POLYGON_Z, SHP_POLYLINE_M] {
                cases.push((
                    format!("poly {shape_type} with {declared} points"),
                    shp_poly_raw(shape_type, &[0], declared, &LINE),
                ));
                cases.push((
                    format!("poly {shape_type} with {declared} parts"),
                    shp_poly_declaring(shape_type, declared, 3, &[0], &LINE),
                ));
            }
        }
        for (what, content) in cases {
            let shp = shp_file(&[content]);
            let (result, peak) = peak_allocation(|| parse_shapefile_bytes(&shp, None));
            assert!(result.is_err(), "{what}");
            assert!(
                peak < 64 << 10,
                "{what}: held {peak} bytes for a {} byte file",
                shp.len()
            );
        }
    });
}

/// Records whose shapes ran past their own length were read to the end of the
/// file and the cursor then moved back, so 12-byte records each re-read the
/// whole tail: output grew with the square of the input.
#[test]
fn shp_overlapping_records_cannot_amplify() {
    guarded("shp overlapping records", SINGLE_INPUT_DEADLINE, || {
        const RECORDS: usize = 2_000;
        const RECORD_BYTES: usize = 12;
        // From a record's start: 8 header bytes, the shape type, the box, both
        // counts, and the five part offsets its part count field (the shape
        // type of the record three further on) declares.
        const POINTS_OFFSET: usize = 8 + 4 + 32 + 4 + 4 + 5 * 4;
        let len = 100 + RECORDS * RECORD_BYTES;
        let mut shp = shp_header(RECORDS * RECORD_BYTES);
        for record in 0..RECORDS {
            // This record's number field is read, little-endian, as the point
            // count of the record four places back: as many as still fit.
            let reader_start = 100 + record.saturating_sub(4) * RECORD_BYTES;
            let points = len.saturating_sub(reader_start + POINTS_OFFSET) / 16;
            shp.extend_from_slice(&(points as i32).to_le_bytes());
            shp.extend_from_slice(&2i32.to_be_bytes());
            shp.extend_from_slice(&SHP_POLYGON.to_le_bytes());
        }
        assert_eq!(shp.len(), len);

        let (result, peak) = peak_allocation(|| parse_shapefile_bytes(&shp, None));
        assert!(result.is_err(), "a 4-byte record cannot hold a polygon");
        assert!(peak <= 64 * len, "held {peak} bytes for a {len} byte file");
    });
}

/// Part offsets that go back to zero made every other part a copy of the
/// whole point array.
#[test]
fn shp_repeated_part_offsets_cannot_amplify() {
    guarded("shp repeated part offsets", SINGLE_INPUT_DEADLINE, || {
        const COUNT: usize = 2_000;
        let points = vec![[1.0, 2.0]; COUNT];
        let starts: Vec<i32> = (0..COUNT)
            .map(|part| if part % 2 == 0 { 0 } else { COUNT as i32 })
            .collect();
        let shp = shp_file(&[shp_poly_raw(SHP_POLYGON, &starts, COUNT as i32, &points)]);

        let (result, peak) = peak_allocation(|| parse_shapefile_bytes(&shp, None));
        assert!(result.is_err(), "part offsets must not decrease");
        assert!(
            peak <= 64 * shp.len(),
            "held {peak} bytes for a {} byte file",
            shp.len()
        );
    });
}

// ── GeoTIFF and thermal regressions ──────────────────────────────────────────

/// A pixel-scale tag with one value was indexed at `[1]`.
#[test]
fn geotiff_short_pixel_scale_does_not_panic() {
    guarded("geotiff pixel scale", SINGLE_INPUT_DEADLINE, || {
        for values in [&[0.25][..], &[]] {
            let tiff = Tiff::image(4, 4, &[8], 1)
                .georeferenced()
                .doubles(TAG_MODEL_PIXEL_SCALE, values)
                .build(&gray_ramp());
            if let Ok(metadata) = parse_geotiff_metadata_reader(Cursor::new(tiff)) {
                assert_eq!(metadata.pixel_scale, None);
                assert_eq!(metadata.bbox, None);
            }
        }
    });
}

/// `width * height` was multiplied as `u32` before anything was decoded.
#[test]
fn geotiff_dimension_overflow_is_rejected() {
    guarded("geotiff dimension overflow", SINGLE_INPUT_DEADLINE, || {
        let scratch = tempfile::NamedTempFile::new().expect("temporary file");
        for (width, height) in [(65_536, 65_536), (u32::MAX, u32::MAX), (u32::MAX, 2)] {
            let tiff = Tiff::image(width, height, &[8], 1)
                .georeferenced()
                .build(&gray_ramp());
            assert!(
                bands_from_bytes(&scratch, &tiff).is_err(),
                "{width}x{height}"
            );
            assert!(
                ThermalParser::parse_thermal_tiff(&tiff).is_err(),
                "{width}x{height}"
            );
            // Metadata alone needs no pixels, so it may succeed.
            let _ = parse_geotiff_metadata_reader(Cursor::new(tiff));
        }
    });
}

/// A few hundred bytes claiming a 16000×16000 raster were given the whole
/// 256 MiB output buffer before the first strip was read.
#[test]
fn tiff_raster_larger_than_the_file_can_hold_is_rejected() {
    guarded("tiff raster size", SINGLE_INPUT_DEADLINE, || {
        let scratch = tempfile::NamedTempFile::new().expect("temporary file");
        for (bits, compression) in [(8u16, 1u16), (8, 5), (8, 8), (8, 32773), (16, 1)] {
            let side = if bits == 8 { 16_000 } else { 11_000 };
            let tiff = Tiff::image(side, side, &[bits], compression)
                .georeferenced()
                .build(&gray_ramp());
            let what = format!("{side}x{side}, {bits} bit, compression {compression}");

            let (result, peak) = peak_allocation(|| bands_from_bytes(&scratch, &tiff));
            assert!(result.is_err(), "{what}");
            assert!(
                peak < 8 << 20,
                "{what}: bands held {peak} bytes for {} bytes",
                tiff.len()
            );

            let (result, peak) = peak_allocation(|| ThermalParser::parse_thermal_tiff(&tiff));
            assert!(result.is_err(), "{what}");
            assert!(
                peak < 8 << 20,
                "{what}: thermal held {peak} bytes for {} bytes",
                tiff.len()
            );
        }
    });
}

/// While it opens a file the decoder reserves memory for a tag's values from
/// the count in its directory entry, under limits the caller cannot set yet:
/// one inflated count in a 400-byte file reserved up to 256 MiB.
#[test]
fn tiff_tag_count_beyond_the_file_is_rejected() {
    guarded("tiff tag count", SINGLE_INPUT_DEADLINE, || {
        let scratch = tempfile::NamedTempFile::new().expect("temporary file");
        for (tag, field_type, count) in [
            (TAG_BITS_PER_SAMPLE, TIFF_SHORT, 8_000_000),
            (TAG_SAMPLE_FORMAT, TIFF_SHORT, 8_000_000),
            (TAG_MODEL_TIEPOINT, TIFF_DOUBLE, 8_000_000),
            (TAG_GEO_KEY_DIRECTORY, TIFF_SHORT, 8_000_000),
            (TAG_GEO_ASCII_PARAMS, TIFF_ASCII, 200_000_000),
            (TAG_GEO_ASCII_PARAMS, TIFF_ASCII, u32::MAX),
        ] {
            // Ten bytes of value: stored out of line in both containers.
            let image = Tiff::image(4, 4, &[8], 1).georeferenced().raw(
                tag,
                field_type,
                count,
                vec![8, 0, 8, 0, 8, 0, 8, 0, 8, 0],
            );
            for (container, tiff) in [
                ("TIFF", image.clone().build(&gray_ramp())),
                ("BigTIFF", image.build_big(&gray_ramp())),
            ] {
                let what = format!("{container} tag {tag} with {count} values");

                let (result, peak) =
                    peak_allocation(|| parse_geotiff_metadata_reader(Cursor::new(&tiff)));
                assert!(result.is_err(), "{what}");
                assert!(peak < 1 << 20, "{what}: metadata held {peak} bytes");

                let (result, peak) = peak_allocation(|| ThermalParser::parse_thermal_tiff(&tiff));
                assert!(result.is_err(), "{what}");
                assert!(peak < 1 << 20, "{what}: thermal held {peak} bytes");

                let (result, peak) = peak_allocation(|| bands_from_bytes(&scratch, &tiff));
                assert!(result.is_err(), "{what}");
                assert!(peak < 1 << 20, "{what}: bands held {peak} bytes");
            }
        }
    });
}

/// The GeoKey directory's key count sized a hash map before the keys were
/// checked against the directory's real length.
#[test]
fn geotiff_geokey_count_is_bounded_by_the_directory() {
    guarded("geotiff geokey count", SINGLE_INPUT_DEADLINE, || {
        let tiff = Tiff::image(4, 4, &[8], 1)
            .georeferenced()
            .shorts(
                TAG_GEO_KEY_DIRECTORY,
                &[1, 1, 0, u16::MAX, 2048, 0, 1, 4326],
            )
            .build(&gray_ramp());
        let (result, peak) =
            peak_allocation(|| parse_geotiff_metadata_reader(Cursor::new(tiff.as_slice())));
        let metadata = result.expect("the one key that is present is still read");
        assert_eq!(metadata.epsg_code, Some(4326));
        assert_eq!(metadata.geokeys.len(), 1);
        assert!(
            peak < 256 << 10,
            "held {peak} bytes for a {} byte file",
            tiff.len()
        );
    });
}

/// `width * height` from the FLIR record header was multiplied as `u32`.
#[test]
fn flir_dimension_overflow_is_rejected() {
    guarded("flir dimension overflow", SINGLE_INPUT_DEADLINE, || {
        for (width, height) in [
            (65_536, 65_536),
            (u32::MAX, u32::MAX),
            (u32::MAX, 2),
            (4, 4),
        ] {
            let rjpeg = flir_rjpeg(width, height, 12);
            let (result, peak) = peak_allocation(|| ThermalParser::parse_flir_rjpeg(&rjpeg));
            assert!(result.is_err(), "{width}x{height} samples do not fit in 12");
            assert!(
                peak < 64 << 10,
                "held {peak} bytes for a {} byte file",
                rjpeg.len()
            );
        }
    });
}

// ── ENVI regressions ─────────────────────────────────────────────────────────

/// The header offset sliced the data without a bounds check.
#[test]
fn envi_header_offset_beyond_the_data_is_rejected() {
    guarded("envi header offset", SINGLE_INPUT_DEADLINE, || {
        let data = envi_data(4);
        // 144 is exactly the end of the data: nothing left to read.
        for offset in ["144", "145", "4096", "18446744073709551615"] {
            let header = envi_header_text(["4", "3", "3"], offset, "4", "bsq", "0");
            let cube = HyperspectralParser::parse_envi_header(&header).expect("header parses");
            assert_eq!(cube.header_offset.to_string(), offset);
            for band in 0..3 {
                assert!(
                    HyperspectralParser::read_band(&data, &cube, band).is_err(),
                    "header offset {offset} with {} data bytes",
                    data.len()
                );
            }
        }
    });
}

/// `lines * samples` was multiplied as `u32`, then sized the output vector.
#[test]
fn envi_dimensions_beyond_the_data_are_rejected() {
    guarded("envi dimensions", SINGLE_INPUT_DEADLINE, || {
        let data = envi_data(8);
        for dimensions in [
            ["65536", "65536", "3"],
            ["4294967295", "4294967295", "4294967295"],
            ["40000", "40000", "3"],
            ["4", "4294967295", "3"],
            ["4294967295", "3", "3"],
            ["4", "3", "4294967295"],
        ] {
            for interleave in ["bsq", "bil", "bip"] {
                for data_type in ["1", "5", "15"] {
                    let header = envi_header_text(dimensions, "0", data_type, interleave, "0");
                    let cube =
                        HyperspectralParser::parse_envi_header(&header).expect("header parses");
                    let what = format!("{dimensions:?} {interleave} type {data_type}");
                    for band in [0, 2, 40, cube.bands - 1, u32::MAX] {
                        let (result, peak) =
                            peak_allocation(|| HyperspectralParser::read_band(&data, &cube, band));
                        // A band-sequential file with a wrong band count still
                        // holds its first bands whole; everything else needs
                        // bytes the 288-byte data does not have.
                        let present = dimensions == ["4", "3", "4294967295"]
                            && interleave == "bsq"
                            && (band as usize + 1) * 12 * cube.data_type.byte_size() <= data.len();
                        assert_eq!(result.is_ok(), present, "{what}, band {band}");
                        assert!(peak < 64 << 10, "{what}, band {band}: held {peak} bytes");
                    }
                }
            }
        }
    });
}

// ── Geometry helpers on hostile coordinates ──────────────────────────────────

/// The antimeridian split unwrapped longitudes by subtracting 360 in a loop.
/// For a large finite longitude `lon - 360.0 == lon`, so the loop never ended.
#[test]
fn huge_finite_longitude_does_not_hang_the_antimeridian_split() {
    guarded("huge longitude", SINGLE_INPUT_DEADLINE, || {
        for (longitude, opposite) in [
            ("1e300", "-1e300"),
            ("-1e300", "1e300"),
            ("1.7976931348623157e308", "-1.7976931348623157e308"),
            ("1e17", "-1e17"),
            ("-3.6e16", "3.6e16"),
        ] {
            let geojson = format!(
                r#"{{"type":"FeatureCollection","features":[
                  {{"type":"Feature","properties":{{}},"geometry":{{"type":"Polygon",
                    "coordinates":[[[0.0,0.0],[{longitude},1.0],[10.0,5.0],[0.0,0.0]]]}}}},
                  {{"type":"Feature","properties":{{}},"geometry":{{"type":"LineString",
                    "coordinates":[[{longitude},0.0],[{opposite},1.0],[179.0,2.0],[-179.0,3.0]]}}}},
                  {{"type":"Feature","properties":{{}},"geometry":{{"type":"MultiPolygon",
                    "coordinates":[[[[170.0,0.0],[-170.0,1.0],[{longitude},5.0],[170.0,0.0]]]]}}}}
                ]}}"#
            );
            let features = parse_geojson_str(&geojson).expect("finite coordinates are accepted");
            exercise_features(&features);
        }

        let far = [[0.0, 0.0], [1e300, 1.0], [-1e300, 2.0], [0.0, 0.0]];
        let shp = shp_file(&[
            shp_poly(SHP_POLYGON, &[&far], None, None),
            shp_poly(SHP_POLYLINE, &[&far, &DATELINE_LINE], None, None),
        ]);
        let data = parse_shapefile_bytes(&shp, None).expect("finite coordinates are accepted");
        exercise_features(&data.features);
    });
}

/// The antimeridian split still does its job on ordinary coordinates.
#[test]
fn antimeridian_split_is_unchanged_for_ordinary_longitudes() {
    guarded("ordinary longitudes", SINGLE_INPUT_DEADLINE, || {
        let point = |lon: f64, lat: f64| geo_fast_core::Coordinate::new(lon, lat);

        // A line that steps over the dateline is cut into a west and an east part.
        let crossing = Geometry::Polyline(vec![vec![point(179.0, 10.0), point(-179.0, 12.0)]]);
        let parts = crossing.split_antimeridian();
        assert_eq!(parts.len(), 2);
        assert!(parts.iter().all(|part| !part.crosses_antimeridian()));
        let west = parts[0].bbox().expect("west part");
        let east = parts[1].bbox().expect("east part");
        assert_eq!((west.min_lon, west.max_lon), (179.0, 180.0));
        assert_eq!((east.min_lon, east.max_lon), (-180.0, -179.0));
        assert_eq!(west.max_lat, 11.0);

        // Longitudes written past ±180 are brought back into range unchanged in shape.
        let wrapped = Geometry::Polyline(vec![vec![point(350.0, 0.0), point(370.0, 1.0)]]);
        let parts = wrapped.split_antimeridian();
        assert_eq!(parts.len(), 1);
        let bbox = parts[0].bbox().expect("wrapped line");
        assert_eq!((bbox.min_lon, bbox.max_lon), (-10.0, 10.0));

        // Exactly half a turn apart is not a crossing and is left alone.
        let half_turn = Geometry::Polyline(vec![vec![point(-90.0, 0.0), point(90.0, 0.0)]]);
        assert_eq!(half_turn.split_antimeridian(), vec![half_turn.clone()]);
    });
}

/// The deepest nesting `parse_geojson_str` accepts.
const GEOJSON_NESTING_LIMIT: usize = 32;

/// `serde_json` recurses once per nested array or object and stops only at
/// 128 levels, which a 256 KiB stack does not survive in an unoptimized
/// build. The reader's own limit must hold through parsing, the geometry
/// helpers, serialization and drop, on that stack.
///
/// Frame sizes differ by platform and build profile. On macOS and Linux the
/// accepted depth also fits half of the small stack; that tighter bar is not
/// asserted, because it does not hold on every platform.
#[test]
fn deeply_nested_geojson_is_bounded_on_a_small_stack() {
    guarded("nested geojson", SINGLE_INPUT_DEADLINE, || {
        /// A Feature whose `id` and `properties` are both `properties`.
        fn feature(geometry: &[u8], properties: &[u8]) -> String {
            let mut document = br#"{"type":"Feature","id":"#.to_vec();
            document.extend_from_slice(properties);
            document.extend_from_slice(br#","properties":"#);
            document.extend_from_slice(properties);
            document.extend_from_slice(br#","geometry":"#);
            document.extend_from_slice(geometry);
            document.push(b'}');
            String::from_utf8(document).expect("ASCII document")
        }
        let point = br#"{"type":"Point","coordinates":[1.0,2.0]}"#;

        // Nested values of exactly `levels` levels. Each kind is reached a
        // different way by the JSON parser: array elements, the first member
        // of an object, and a later member of an object.
        let nested_values = |levels: usize| {
            [
                smoke::nested(b"[", b"1.0", b"]", levels),
                smoke::nested(br#"{"a":"#, b"1", b"}", levels),
                smoke::nested(br#"{"t":"x","u":1,"g":"#, b"1", b"}", levels),
                smoke::nested(br#"{"t":"x","u":1,"g":["#, b"1", b"]}", levels / 2),
            ]
        };

        // Inside the limit: the Feature object is one level, so its values
        // may nest one level less. These parse, and everything a consumer
        // then does with the features fits the small stack.
        for value in nested_values(GEOJSON_NESTING_LIMIT - 1) {
            let document = feature(point, &value);
            smoke::run_on_small_stack(move || {
                let features = parse_geojson_str(&document).expect("nesting within the limit");
                assert_eq!(features.len(), 1);
                exercise_features(&features);
                let json = features_to_geojson(&features).expect("serializable");
                assert!(json.to_string().len() > document.len() / 2);
            });
        }

        // Past the limit, in every position a document can nest: rejected,
        // not parsed. Deepest first, on the plain small stack, so that a reader
        // which does recurse into these fails by overflowing it.
        for levels in [
            100_000,
            1_000,
            200,
            129,
            128,
            127,
            GEOJSON_NESTING_LIMIT + 1,
            GEOJSON_NESTING_LIMIT,
        ] {
            let mut documents = Vec::new();
            for value in nested_values(levels + levels % 2) {
                documents.push(feature(point, &value));
                let mut polygon = br#"{"type":"Polygon","coordinates":"#.to_vec();
                polygon.extend_from_slice(&value);
                polygon.push(b'}');
                documents.push(feature(&polygon, b"null"));
            }
            documents.push(feature(
                &smoke::nested(
                    br#"{"type":"GeometryCollection","geometries":["#,
                    point,
                    b"]}",
                    levels,
                ),
                b"null",
            ));
            documents.push(
                String::from_utf8(smoke::nested(
                    br#"{"type":"FeatureCollection","features":["#,
                    feature(point, b"null").as_bytes(),
                    b"]}",
                    levels,
                ))
                .expect("ASCII document"),
            );
            for document in documents {
                smoke::run_on_small_stack(move || {
                    assert!(parse_geojson_str(&document).is_err(), "{levels} levels");
                });
            }
        }

        // Brackets inside strings are text, not nesting.
        let brackets = "[{".repeat(10_000);
        let document = feature(
            point,
            format!(r#"{{"note":"{brackets}\"{brackets}"}}"#).as_bytes(),
        );
        smoke::run_on_small_stack(move || {
            let features = parse_geojson_str(&document).expect("brackets in a string");
            assert_eq!(features.len(), 1);
        });
    });
}

/// Bulk loading and querying with boxes no well-formed file produces.
#[test]
fn spatial_index_survives_degenerate_boxes() {
    guarded("spatial index", SINGLE_INPUT_DEADLINE, || {
        let empty = SpatialIndex::bulk_load(Vec::new());
        assert!(empty.is_empty());
        assert!(empty
            .query(&BBox::new(-180.0, -90.0, 180.0, 90.0))
            .is_empty());

        let extremes = [
            f64::NAN,
            f64::INFINITY,
            f64::NEG_INFINITY,
            f64::MAX,
            f64::MIN,
            f64::MIN_POSITIVE,
            -0.0,
            0.0,
            180.0,
            -180.0,
            1e300,
        ];
        let mut boxes = Vec::new();
        for &a in &extremes {
            for &b in &extremes {
                boxes.push(BBox::new(a, b, b, a));
                boxes.push(BBox::new(a, a, b, b));
                boxes.push(BBox::new(b, a, a, b));
            }
        }
        let items: Vec<(BBox, usize)> = boxes.iter().copied().zip(0..).collect();
        let valid = boxes
            .iter()
            .filter(|bbox| bbox.is_finite() && bbox.is_ordered())
            .count();

        for capacity in [0, 1, 2, 3, 16, items.len(), usize::MAX - 1, usize::MAX] {
            let index = SpatialIndex::bulk_load_with_capacity(items.clone(), capacity);
            assert_eq!(index.len(), valid, "capacity {capacity}");
            let everything = BBox::new(f64::MIN, f64::MIN, f64::MAX, f64::MAX);
            assert_eq!(index.query(&everything).len(), valid, "capacity {capacity}");
            for query in &boxes {
                let hits = index.query(query);
                assert!(hits.len() <= valid);
                let _ = index.query_antimeridian_aware(query);
            }
        }

        // Ten thousand copies of one box: every sort key ties.
        let same = vec![(BBox::new(1.0, 1.0, 1.0, 1.0), 7usize); 10_000];
        let index = SpatialIndex::bulk_load_with_capacity(same, 2);
        assert_eq!(index.query(&BBox::new(1.0, 1.0, 1.0, 1.0)).len(), 10_000);
        assert!(index.query(&BBox::new(2.0, 2.0, 3.0, 3.0)).is_empty());
    });
}

// ── Mutation runs ────────────────────────────────────────────────────────────

#[test]
fn mutated_shp_never_panics() {
    let seeds = vec![shp_plain_seed(), shp_measured_seed()];
    let tables = [dbf_seed(), dbf_seed_with_deleted_row()];
    mutation_run("geo-fast-core shp", seeds, 20_000, move |bytes| {
        if let Ok(data) = parse_shapefile_bytes(bytes, None) {
            exercise_features(&data.features);
        }
        for table in &tables {
            let _ = parse_shapefile_bytes(bytes, Some(table));
        }
    });
}

#[test]
fn mutated_dbf_never_panics() {
    let seeds = vec![dbf_seed(), dbf_seed_with_deleted_row()];
    let shapes = [shp_plain_seed(), shp_measured_seed()];
    mutation_run("geo-fast-core dbf", seeds, 20_000, move |bytes| {
        for shape in &shapes {
            if let Ok(data) = parse_shapefile_bytes(shape, Some(bytes)) {
                let _ = features_to_geojson(&data.features);
            }
        }
    });
}

#[test]
fn mutated_geojson_never_panics() {
    let seeds = vec![
        GEOJSON_SEED.as_bytes().to_vec(),
        GEOJSON_COLLECTION_SEED.as_bytes().to_vec(),
    ];
    mutation_run("geo-fast-core geojson", seeds, 20_000, |bytes| {
        if let Ok(features) = parse_geojson_str(&String::from_utf8_lossy(bytes)) {
            exercise_features(&features);
        }
    });
}

#[test]
fn mutated_tiff_never_panics() {
    let seeds = vec![
        geotiff_gray8_seed(),
        bigtiff_gray8_seed(),
        tiff_gray16_seed(),
        geotiff_rgb8_seed(),
        geotiff_float32_seed(),
        tiff_packbits_seed(),
        tiff_deflate_seed(),
    ];
    let scratch = tempfile::NamedTempFile::new().expect("temporary file");
    mutation_run("geo-fast-core tiff", seeds, 8_000, move |bytes| {
        if let Ok(metadata) = parse_geotiff_metadata_reader(Cursor::new(bytes)) {
            let _ = metadata.upper_left();
            let _ = metadata.lower_right();
        }
        let _ = ThermalParser::parse_thermal_tiff(bytes);
        let _ = bands_from_bytes(&scratch, bytes);
    });
}

#[test]
fn mutated_envi_header_never_panics() {
    let mut seeds = Vec::new();
    for (data_type, interleave, byte_order) in [
        (4u8, "bsq", 0u8),
        (2, "bil", 1),
        (12, "bip", 0),
        (1, "bsq", 0),
        (5, "bil", 0),
        (14, "bip", 1),
        (6, "bsq", 0),
    ] {
        seeds.push(envi_header(4, 3, 3, data_type, interleave, byte_order));
    }
    let data = envi_data(8);
    mutation_run("geo-fast-core envi header", seeds, 30_000, move |bytes| {
        let Ok(cube) = HyperspectralParser::parse_envi_header(bytes) else {
            return;
        };
        for band in [0, 1, 2, cube.bands - 1, cube.bands, u32::MAX] {
            let _ = HyperspectralParser::read_band(&data, &cube, band);
            let _ = HyperspectralParser::read_band(&data[..data.len() / 3], &cube, band);
        }
    });
}

#[test]
fn mutated_envi_data_never_panics() {
    let cubes: Vec<_> = [(4u8, "bsq"), (2, "bil"), (15, "bip")]
        .into_iter()
        .map(|(data_type, interleave)| {
            HyperspectralParser::parse_envi_header(&envi_header(4, 3, 3, data_type, interleave, 1))
                .expect("seed header")
        })
        .collect();
    let seeds = vec![envi_data(2), envi_data(4), envi_data(8)];
    mutation_run("geo-fast-core envi data", seeds, 10_000, move |bytes| {
        for cube in &cubes {
            for band in 0..3 {
                let _ = HyperspectralParser::read_band(bytes, cube, band);
            }
        }
    });
}

#[test]
fn mutated_flir_rjpeg_never_panics() {
    let seeds = vec![
        flir_rjpeg(4, 3, 12),
        flir_rjpeg(0, 0, 12),
        flir_rjpeg(2, 2, 4),
    ];
    mutation_run("geo-fast-core flir", seeds, 30_000, |bytes| {
        let _ = ThermalParser::parse_flir_rjpeg(bytes);
    });
}

#[test]
fn mutated_sar_xml_never_panics() {
    let seeds = vec![
        SAFE_MANIFEST.as_bytes().to_vec(),
        SAR_ANNOTATION.as_bytes().to_vec(),
    ];
    mutation_run("geo-fast-core sar xml", seeds, 20_000, |bytes| {
        let _ = SarParser::parse_safe_manifest(bytes);
        let _ = SarParser::extract_gcps(bytes);
    });
}

#[test]
fn mutated_ceos_leader_never_panics() {
    let seeds = vec![ceos_leader_seed()];
    mutation_run("geo-fast-core ceos", seeds, 20_000, |bytes| {
        let _ = SarParser::parse_ceos_leader(bytes);
    });
}

/// Boxes and queries as the bindings receive them: a WMS bbox string, and
/// raw doubles for the boxes to index.
#[test]
fn mutated_boxes_never_panic() {
    let mut doubles = Vec::new();
    push_f64s(
        &mut doubles,
        &[
            -48.0,
            -16.0,
            -47.0,
            -15.0,
            170.0,
            -10.0,
            -170.0,
            10.0,
            f64::NAN,
            0.0,
            f64::INFINITY,
            1.0,
            0.0,
            0.0,
            0.0,
            0.0,
        ],
    );
    let seeds = vec![
        b"-48.0,-16.0,-47.0,-15.0".to_vec(),
        b" 175 , -5 , -175 , 5 ".to_vec(),
        doubles,
    ];
    mutation_run("geo-fast-core boxes", seeds, 20_000, |bytes| {
        let query = BBox::from_wms_string(&String::from_utf8_lossy(bytes))
            .unwrap_or_else(|_| BBox::new(-180.0, -90.0, 180.0, 90.0));
        let _ = geo_fast_core::bbox_area_km2(&query);
        let values: Vec<f64> = bytes
            .as_chunks::<8>()
            .0
            .iter()
            .map(|chunk| f64::from_le_bytes(*chunk))
            .collect();
        let boxes: Vec<(BBox, usize)> = values
            .as_chunks::<4>()
            .0
            .iter()
            .map(|quad| BBox::new(quad[0], quad[1], quad[2], quad[3]))
            .zip(0..)
            .collect();
        for (bbox, _) in &boxes {
            let _ = geo_fast_core::bbox_iou_geodesic(bbox, &query);
            let _ = bbox.split_antimeridian();
        }
        let index = SpatialIndex::bulk_load_with_capacity(boxes, 2 + bytes.len() % 3);
        let _ = index.query_antimeridian_aware(&query);
    });
}

/// The seeds above are also the starting corpus of the fuzz targets in
/// `fuzz/`; `WRITE_FUZZ_SEEDS=1` writes them out again after a seed changes.
#[test]
fn the_fuzz_corpus_holds_these_seeds() {
    smoke::fuzz_seeds(
        env!("CARGO_MANIFEST_DIR"),
        &[
            ("shapefile", "plain.shp", shp_plain_seed()),
            ("shapefile", "measured.shp", shp_measured_seed()),
            ("shapefile", "table.dbf", dbf_seed()),
            ("shapefile", "deleted-row.dbf", dbf_seed_with_deleted_row()),
            ("geotiff", "gray8.tif", geotiff_gray8_seed()),
            ("geotiff", "bigtiff-gray8.tif", bigtiff_gray8_seed()),
            ("geotiff", "gray16.tif", tiff_gray16_seed()),
            ("geotiff", "rgb8.tif", geotiff_rgb8_seed()),
            ("geotiff", "float32.tif", geotiff_float32_seed()),
            ("geotiff", "packbits.tif", tiff_packbits_seed()),
            ("geotiff", "deflate.tif", tiff_deflate_seed()),
            (
                "geojson",
                "geometries.geojson",
                GEOJSON_SEED.as_bytes().to_vec(),
            ),
            (
                "geojson",
                "feature.geojson",
                GEOJSON_COLLECTION_SEED.as_bytes().to_vec(),
            ),
        ],
    );
}

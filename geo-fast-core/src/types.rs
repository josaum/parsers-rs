//! Shared GIS types used across all format parsers.
//!
//! # Coordinate system
//!
//! Unless a format module documents otherwise, coordinates are **EPSG:4326**
//! (WGS84) with axis order **(longitude, latitude)** in decimal degrees —
//! matching GeoJSON RFC 7946, not EPSG:4326's official lat/lon axis order.
//!
//! # Bounding-box edge semantics
//!
//! [`BBox`] point containment and intersection use **closed / inclusive**
//! edges on all four sides (`min <= x <= max`). Degenerate boxes (zero width
//! or height) are allowed; their area is `0.0` and IoU against any other box
//! is `0.0` unless both boxes are identical points/lines (still `0.0` area).

use serde::{Deserialize, Serialize};
use std::fmt;

/// A single 2-D geographic coordinate (longitude, latitude in decimal degrees).
///
/// Coordinates use the WGS84 datum (EPSG:4326) unless otherwise specified.
/// Axis order is always `(lon, lat)` — GeoJSON / WMS convention.
///
/// # Examples
///
/// ```
/// use geo_fast_core::Coordinate;
///
/// let coord = Coordinate::new(-47.9297, -15.7801);
/// assert!(coord.is_valid());
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Coordinate {
    /// Longitude in decimal degrees (-180 to 180).
    pub lon: f64,
    /// Latitude in decimal degrees (-90 to 90).
    pub lat: f64,
}

impl Coordinate {
    /// Creates a new coordinate.
    ///
    /// # Example
    /// ```
    /// use geo_fast_core::Coordinate;
    /// let coord = Coordinate::new(-47.9297, -15.7801); // Brasília
    /// ```
    pub fn new(lon: f64, lat: f64) -> Self {
        Coordinate { lon, lat }
    }

    /// Returns true if both components are finite (not NaN / ±Inf).
    pub fn is_finite(&self) -> bool {
        self.lon.is_finite() && self.lat.is_finite()
    }

    /// Returns true if the coordinate is finite and within valid WGS84 bounds.
    ///
    /// Longitude must be in [-180, 180] and latitude in [-90, 90].
    /// NaN / Inf fail this check (unlike a bare range compare, which is true
    /// for NaN under some accidental patterns).
    pub fn is_valid(&self) -> bool {
        self.is_finite()
            && self.lon >= -180.0
            && self.lon <= 180.0
            && self.lat >= -90.0
            && self.lat <= 90.0
    }

    /// Validates the coordinate, returning an error if out of bounds or non-finite.
    pub fn validate(&self) -> crate::Result<()> {
        if !self.is_valid() {
            return Err(crate::GeoError::Validation(format!(
                "Coordinate ({}, {}) is out of valid bounds or non-finite",
                self.lon, self.lat
            )));
        }
        Ok(())
    }

    /// Returns the coordinate as a tuple (lon, lat).
    pub fn as_tuple(&self) -> (f64, f64) {
        (self.lon, self.lat)
    }

    /// Calculates the approximate distance to another coordinate in kilometers.
    ///
    /// Uses the Haversine formula for great-circle distance on a sphere.
    /// Returns `f64::NAN` if either coordinate is non-finite.
    pub fn distance_km(&self, other: &Coordinate) -> f64 {
        if !self.is_finite() || !other.is_finite() {
            return f64::NAN;
        }

        const EARTH_RADIUS_KM: f64 = 6371.0;

        let d_lat = (other.lat - self.lat).to_radians();
        let d_lon = (other.lon - self.lon).to_radians();

        let a = (d_lat / 2.0).sin().powi(2)
            + self.lat.to_radians().cos()
                * other.lat.to_radians().cos()
                * (d_lon / 2.0).sin().powi(2);

        let c = 2.0 * a.sqrt().atan2((1.0 - a).sqrt());

        EARTH_RADIUS_KM * c
    }
}

impl fmt::Display for Coordinate {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "({}, {})", self.lon, self.lat)
    }
}

impl From<(f64, f64)> for Coordinate {
    fn from((lon, lat): (f64, f64)) -> Self {
        Coordinate::new(lon, lat)
    }
}

impl From<[f64; 2]> for Coordinate {
    fn from([lon, lat]: [f64; 2]) -> Self {
        Coordinate::new(lon, lat)
    }
}

/// Alias for `Coordinate` for backward compatibility.
pub type GeoPoint = Coordinate;

/// Bounding box in geographic coordinates (EPSG:4326 / lon-lat).
///
/// The box is defined by its minimum and maximum longitude and latitude.
/// Use [`BBox::is_valid`] to ensure finite values and `min <= max` per axis.
///
/// Edge semantics are **closed / inclusive** on all four sides. See the
/// module-level docs for coordinate-system notes.
///
/// Antimeridian-crossing boxes (`min_lon > max_lon`) are detectable via
/// [`BBox::crosses_antimeridian`], but spatial predicates (`intersects`,
/// `contains`, `iou`, …) assume a **normalized** non-wrapping box. Call
/// [`BBox::normalized`] first when corners may be unordered.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct BBox {
    /// Minimum longitude (western bound).
    pub min_lon: f64,
    /// Maximum longitude (eastern bound).
    pub max_lon: f64,
    /// Minimum latitude (southern bound).
    pub min_lat: f64,
    /// Maximum latitude (northern bound).
    pub max_lat: f64,
}

impl BBox {
    /// Creates a new bounding box.
    ///
    /// # Example
    /// ```
    /// use geo_fast_core::BBox;
    /// let bbox = BBox::new(-48.0, -16.0, -47.0, -15.0);
    /// ```
    pub fn new(min_lon: f64, min_lat: f64, max_lon: f64, max_lat: f64) -> Self {
        BBox {
            min_lon,
            max_lon,
            min_lat,
            max_lat,
        }
    }

    /// Creates a bounding box from two corner coordinates.
    pub fn from_corners(c1: Coordinate, c2: Coordinate) -> Self {
        BBox {
            min_lon: c1.lon.min(c2.lon),
            max_lon: c1.lon.max(c2.lon),
            min_lat: c1.lat.min(c2.lat),
            max_lat: c1.lat.max(c2.lat),
        }
    }

    /// Creates a bounding box centered at a point with given width and height in degrees.
    pub fn centered_at(center: Coordinate, width_deg: f64, height_deg: f64) -> Self {
        let half_w = width_deg / 2.0;
        let half_h = height_deg / 2.0;
        BBox {
            min_lon: center.lon - half_w,
            max_lon: center.lon + half_w,
            min_lat: center.lat - half_h,
            max_lat: center.lat + half_h,
        }
    }

    /// Empty / invalid seed for incremental expansion via [`BBox::expand_to_include`].
    ///
    /// Uses `±INFINITY` so the first finite point becomes the sole extent.
    pub fn empty() -> Self {
        BBox {
            min_lon: f64::INFINITY,
            max_lon: f64::NEG_INFINITY,
            min_lat: f64::INFINITY,
            max_lat: f64::NEG_INFINITY,
        }
    }

    /// Returns `(min_lon, min_lat, max_lon, max_lat)` as a tuple.
    pub fn as_tuple(&self) -> (f64, f64, f64, f64) {
        (self.min_lon, self.min_lat, self.max_lon, self.max_lat)
    }

    /// Returns the four corners as [southwest, southeast, northeast, northwest].
    pub fn corners(&self) -> [Coordinate; 4] {
        [
            Coordinate::new(self.min_lon, self.min_lat),
            Coordinate::new(self.max_lon, self.min_lat),
            Coordinate::new(self.max_lon, self.max_lat),
            Coordinate::new(self.min_lon, self.max_lat),
        ]
    }

    /// Returns the center point of the bounding box.
    pub fn center(&self) -> Coordinate {
        Coordinate::new(
            (self.min_lon + self.max_lon) / 2.0,
            (self.min_lat + self.max_lat) / 2.0,
        )
    }

    /// Returns the width in degrees (longitude span).
    pub fn width(&self) -> f64 {
        self.max_lon - self.min_lon
    }

    /// Returns the height in degrees (latitude span).
    pub fn height(&self) -> f64 {
        self.max_lat - self.min_lat
    }

    /// Axis-aligned area in square degrees (`width * height`).
    ///
    /// Returns `0.0` for non-finite or inverted boxes. This is **not**
    /// geodesic area — use only for relative overlap / IoU in lon-lat space.
    pub fn area(&self) -> f64 {
        if !self.is_finite() || !self.is_ordered() {
            return 0.0;
        }
        let w = self.width();
        let h = self.height();
        if w <= 0.0 || h <= 0.0 {
            0.0
        } else {
            w * h
        }
    }

    /// Returns true if all four components are finite.
    pub fn is_finite(&self) -> bool {
        self.min_lon.is_finite()
            && self.max_lon.is_finite()
            && self.min_lat.is_finite()
            && self.max_lat.is_finite()
    }

    /// Returns true if `min <= max` on both axes (ignores finiteness).
    pub fn is_ordered(&self) -> bool {
        self.min_lon <= self.max_lon && self.min_lat <= self.max_lat
    }

    /// Returns true if the bounding box is finite and ordered (`min <= max`).
    pub fn is_valid(&self) -> bool {
        self.is_finite() && self.is_ordered()
    }

    /// Returns true if the box has zero area (point or line segment extent).
    pub fn is_degenerate(&self) -> bool {
        !self.is_valid() || self.width() <= 0.0 || self.height() <= 0.0
    }

    /// Normalizes the bounding box by ensuring min <= max for each dimension.
    ///
    /// This is useful when corners are given in arbitrary order. Does **not**
    /// unwrap antimeridian-crossing extents — those remain `min_lon > max_lon`
    /// only when constructed that way intentionally; unordered corners that
    /// merely swap axes are fixed here.
    pub fn normalize(&mut self) {
        if self.min_lon > self.max_lon {
            std::mem::swap(&mut self.min_lon, &mut self.max_lon);
        }
        if self.min_lat > self.max_lat {
            std::mem::swap(&mut self.min_lat, &mut self.max_lat);
        }
    }

    /// Returns a normalized copy of the bounding box.
    pub fn normalized(&self) -> Self {
        let mut bbox = *self;
        bbox.normalize();
        bbox
    }

    /// Returns true if this bounding box intersects with another (inclusive edges).
    ///
    /// Non-finite boxes never intersect.
    pub fn intersects(&self, other: &BBox) -> bool {
        if !self.is_finite() || !other.is_finite() {
            return false;
        }
        self.min_lon <= other.max_lon
            && self.max_lon >= other.min_lon
            && self.min_lat <= other.max_lat
            && self.max_lat >= other.min_lat
    }

    /// Returns true if this bounding box contains a point (inclusive edges).
    ///
    /// Non-finite points or boxes return `false`.
    pub fn contains_point(&self, point: &Coordinate) -> bool {
        if !self.is_finite() || !point.is_finite() {
            return false;
        }
        point.lon >= self.min_lon
            && point.lon <= self.max_lon
            && point.lat >= self.min_lat
            && point.lat <= self.max_lat
    }

    /// Returns true if this bounding box fully contains another (inclusive edges).
    pub fn contains(&self, other: &BBox) -> bool {
        if !self.is_finite() || !other.is_finite() {
            return false;
        }
        other.min_lon >= self.min_lon
            && other.max_lon <= self.max_lon
            && other.min_lat >= self.min_lat
            && other.max_lat <= self.max_lat
    }

    /// Computes the intersection of two bounding boxes.
    ///
    /// Returns `None` if they don't intersect or either box is non-finite.
    pub fn intersection(&self, other: &BBox) -> Option<BBox> {
        if !self.intersects(other) {
            return None;
        }
        Some(BBox::new(
            self.min_lon.max(other.min_lon),
            self.min_lat.max(other.min_lat),
            self.max_lon.min(other.max_lon),
            self.max_lat.min(other.max_lat),
        ))
    }

    /// Intersection-over-union in lon-lat square degrees.
    ///
    /// Returns `0.0` when either box is non-finite, non-overlapping, or has
    /// zero area. Useful for raster/vector tile overlap scoring — not a
    /// geodesic IoU.
    pub fn iou(&self, other: &BBox) -> f64 {
        let a = self.area();
        let b = other.area();
        if a <= 0.0 || b <= 0.0 {
            return 0.0;
        }
        match self.intersection(other) {
            Some(inter) => {
                let inter_area = inter.area();
                if inter_area <= 0.0 {
                    0.0
                } else {
                    inter_area / (a + b - inter_area)
                }
            }
            None => 0.0,
        }
    }

    /// Expands this bounding box to include a point.
    ///
    /// Non-finite points are ignored so a NaN coordinate cannot poison the box.
    pub fn expand_to_include(&mut self, point: &Coordinate) {
        if !point.is_finite() {
            return;
        }
        self.min_lon = self.min_lon.min(point.lon);
        self.max_lon = self.max_lon.max(point.lon);
        self.min_lat = self.min_lat.min(point.lat);
        self.max_lat = self.max_lat.max(point.lat);
    }

    /// Merges another bounding box into this one.
    ///
    /// Non-finite `other` is ignored.
    pub fn merge(&mut self, other: &BBox) {
        if !other.is_finite() {
            return;
        }
        // If `self` is still the empty seed, adopt `other` wholesale.
        if !self.is_finite() {
            *self = *other;
            return;
        }
        self.min_lon = self.min_lon.min(other.min_lon);
        self.max_lon = self.max_lon.max(other.max_lon);
        self.min_lat = self.min_lat.min(other.min_lat);
        self.max_lat = self.max_lat.max(other.max_lat);
    }

    /// Returns a new bounding box that is the union of this and another.
    pub fn union(&self, other: &BBox) -> BBox {
        if !self.is_finite() {
            return *other;
        }
        if !other.is_finite() {
            return *self;
        }
        BBox::new(
            self.min_lon.min(other.min_lon),
            self.min_lat.min(other.min_lat),
            self.max_lon.max(other.max_lon),
            self.max_lat.max(other.max_lat),
        )
    }

    /// WMS-style comma-separated string `"min_lon,min_lat,max_lon,max_lat"`.
    pub fn to_wms_string(&self) -> String {
        format!(
            "{},{},{},{}",
            self.min_lon, self.min_lat, self.max_lon, self.max_lat
        )
    }

    /// Parse a WMS-style bbox string.
    ///
    /// Format: "min_lon,min_lat,max_lon,max_lat"
    pub fn from_wms_string(s: &str) -> crate::Result<Self> {
        let mut parts = s.split(',');
        let min_lon = parts
            .next()
            .ok_or_else(|| crate::GeoError::InvalidFormat("Invalid min_lon".into()))?
            .trim()
            .parse::<f64>()
            .map_err(|_| crate::GeoError::InvalidFormat("Invalid min_lon".into()))?;
        let min_lat = parts
            .next()
            .ok_or_else(|| crate::GeoError::InvalidFormat("Invalid min_lat".into()))?
            .trim()
            .parse::<f64>()
            .map_err(|_| crate::GeoError::InvalidFormat("Invalid min_lat".into()))?;
        let max_lon = parts
            .next()
            .ok_or_else(|| crate::GeoError::InvalidFormat("Invalid max_lon".into()))?
            .trim()
            .parse::<f64>()
            .map_err(|_| crate::GeoError::InvalidFormat("Invalid max_lon".into()))?;
        let max_lat = parts
            .next()
            .ok_or_else(|| crate::GeoError::InvalidFormat("Invalid max_lat".into()))?
            .trim()
            .parse::<f64>()
            .map_err(|_| crate::GeoError::InvalidFormat("Invalid max_lat".into()))?;
        if parts.next().is_some() {
            return Err(crate::GeoError::InvalidFormat(
                "WMS bbox must have 4 comma-separated values".into(),
            ));
        }
        if ![min_lon, min_lat, max_lon, max_lat]
            .iter()
            .all(|v| v.is_finite())
        {
            return Err(crate::GeoError::InvalidFormat(
                "WMS bbox values must be finite".into(),
            ));
        }
        Ok(BBox::new(min_lon, min_lat, max_lon, max_lat))
    }

    /// Returns true if the bounding box crosses the antimeridian (180°/-180°).
    ///
    /// Detected as `min_lon > max_lon` after construction. Spatial predicates
    /// do **not** unwrap this case — normalize or split first via
    /// [`BBox::split_antimeridian`].
    pub fn crosses_antimeridian(&self) -> bool {
        self.is_finite() && self.min_lon > self.max_lon
    }

    /// Split an antimeridian-crossing box into western and eastern halves.
    ///
    /// - Non-wrapping boxes return a single-element vec with `*self`.
    /// - Wrapping boxes (`min_lon > max_lon`) become
    ///   `[min_lon, 180] ∪ [-180, max_lon]` at the same latitudes.
    ///
    /// Use this before [`BBox::intersects`] / [`BBox::iou`] / STRtree queries
    /// when extents may wrap the dateline.
    pub fn split_antimeridian(&self) -> Vec<BBox> {
        if !self.is_finite() {
            return Vec::new();
        }
        if !self.crosses_antimeridian() {
            return vec![*self];
        }
        vec![
            BBox::new(self.min_lon, self.min_lat, 180.0, self.max_lat),
            BBox::new(-180.0, self.min_lat, self.max_lon, self.max_lat),
        ]
    }

    /// Inclusive-edge intersection that understands antimeridian wrapping.
    ///
    /// Returns `true` if any split part of `self` intersects any split part
    /// of `other`.
    pub fn intersects_antimeridian_aware(&self, other: &BBox) -> bool {
        let a = self.split_antimeridian();
        let b = other.split_antimeridian();
        for pa in &a {
            for pb in &b {
                if pa.intersects(pb) {
                    return true;
                }
            }
        }
        false
    }

    /// Point-in-bbox that understands antimeridian wrapping.
    pub fn contains_point_antimeridian_aware(&self, point: &Coordinate) -> bool {
        self.split_antimeridian()
            .iter()
            .any(|part| part.contains_point(point))
    }

    /// Converts the bounding box to an array [min_lon, min_lat, max_lon, max_lat].
    pub fn to_array(&self) -> [f64; 4] {
        [self.min_lon, self.min_lat, self.max_lon, self.max_lat]
    }
}

impl Default for BBox {
    fn default() -> Self {
        BBox::new(0.0, 0.0, 0.0, 0.0)
    }
}

impl fmt::Display for BBox {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "BBox({}, {} → {}, {})",
            self.min_lon, self.min_lat, self.max_lon, self.max_lat
        )
    }
}

/// Geometry variants supported by the parsers.
///
/// Axis order is always `(lon, lat)`. Polygon rings follow GeoJSON: the first
/// ring is the exterior, subsequent rings are holes. [`Geometry::MultiPolygon`]
/// preserves per-polygon ring groups (unlike the previous flatten-on-parse
/// path that collapsed MultiPolygon into a single [`Geometry::Polygon`]).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", content = "coordinates")]
#[non_exhaustive]
pub enum Geometry {
    /// A single point.
    Point(Coordinate),
    /// A polyline: outer Vec = parts, inner Vec = ring vertices.
    Polyline(Vec<Vec<Coordinate>>),
    /// A polygon: outer Vec = rings, inner Vec = ring vertices.
    /// First ring is the exterior, subsequent rings are holes.
    Polygon(Vec<Vec<Coordinate>>),
    /// Multiple points.
    MultiPoint(Vec<Coordinate>),
    /// Multiple polygons: outer Vec = polygons, each polygon is rings of vertices.
    MultiPolygon(Vec<Vec<Vec<Coordinate>>>),
}

impl Geometry {
    /// Returns the first finite coordinate (or the point itself).
    pub fn representative_point(&self) -> Option<Coordinate> {
        match self {
            Geometry::Point(p) => p.is_finite().then_some(*p),
            Geometry::MultiPoint(pts) => pts.iter().copied().find(|p| p.is_finite()),
            Geometry::Polyline(parts) => parts
                .iter()
                .flat_map(|p| p.iter())
                .copied()
                .find(|p| p.is_finite()),
            Geometry::Polygon(rings) => rings
                .iter()
                .flat_map(|r| r.iter())
                .copied()
                .find(|p| p.is_finite()),
            Geometry::MultiPolygon(polys) => polys
                .iter()
                .flat_map(|poly| poly.iter())
                .flat_map(|r| r.iter())
                .copied()
                .find(|p| p.is_finite()),
        }
    }

    /// Compute a simple axis-aligned bounding box over all finite coordinates.
    ///
    /// Allocation-free single pass. Returns `None` when there are no finite
    /// coordinates (empty geometry or all-NaN).
    pub fn bbox(&self) -> Option<BBox> {
        let mut bbox = BBox::empty();
        let mut any = false;
        self.for_each_coordinate(|p| {
            if p.is_finite() {
                bbox.expand_to_include(p);
                any = true;
            }
        });
        any.then_some(bbox)
    }

    /// Invoke `f` for every coordinate without allocating.
    pub fn for_each_coordinate<F>(&self, mut f: F)
    where
        F: FnMut(&Coordinate),
    {
        match self {
            Geometry::Point(p) => f(p),
            Geometry::MultiPoint(pts) => {
                for p in pts {
                    f(p);
                }
            }
            Geometry::Polyline(parts) => {
                for part in parts {
                    for p in part {
                        f(p);
                    }
                }
            }
            Geometry::Polygon(rings) => {
                for ring in rings {
                    for p in ring {
                        f(p);
                    }
                }
            }
            Geometry::MultiPolygon(polys) => {
                for poly in polys {
                    for ring in poly {
                        for p in ring {
                            f(p);
                        }
                    }
                }
            }
        }
    }

    /// Returns the geometry type name as a string.
    pub fn geometry_type(&self) -> &'static str {
        match self {
            Geometry::Point(_) => "Point",
            Geometry::MultiPoint(_) => "MultiPoint",
            Geometry::Polyline(_) => "Polyline",
            Geometry::Polygon(_) => "Polygon",
            Geometry::MultiPolygon(_) => "MultiPolygon",
        }
    }

    /// Returns true if the geometry contains any coordinates.
    pub fn is_empty(&self) -> bool {
        match self {
            Geometry::Point(_) => false,
            Geometry::MultiPoint(pts) => pts.is_empty(),
            Geometry::Polyline(parts) => parts.is_empty() || parts.iter().all(|p| p.is_empty()),
            Geometry::Polygon(rings) => rings.is_empty() || rings.iter().all(|r| r.is_empty()),
            Geometry::MultiPolygon(polys) => {
                polys.is_empty()
                    || polys
                        .iter()
                        .all(|poly| poly.is_empty() || poly.iter().all(|r| r.is_empty()))
            }
        }
    }

    /// Returns the total number of coordinates in the geometry.
    pub fn coordinate_count(&self) -> usize {
        match self {
            Geometry::Point(_) => 1,
            Geometry::MultiPoint(pts) => pts.len(),
            Geometry::Polyline(parts) => parts.iter().map(|p| p.len()).sum(),
            Geometry::Polygon(rings) => rings.iter().map(|r| r.len()).sum(),
            Geometry::MultiPolygon(polys) => polys
                .iter()
                .map(|poly| poly.iter().map(|r| r.len()).sum::<usize>())
                .sum(),
        }
    }

    /// True when any consecutive lon pair jumps more than 180° (dateline edge).
    ///
    /// Detects rings/lines that cross the antimeridian in GeoJSON-style
    /// coordinates. Does not rewrite the geometry — use
    /// [`Geometry::split_antimeridian`] to produce non-wrapping parts.
    pub fn crosses_antimeridian(&self) -> bool {
        match self {
            Geometry::Point(_) | Geometry::MultiPoint(_) => false,
            Geometry::Polyline(parts) => parts.iter().any(|p| ring_crosses_antimeridian(p)),
            Geometry::Polygon(rings) => rings.iter().any(|r| ring_crosses_antimeridian(r)),
            Geometry::MultiPolygon(polys) => polys
                .iter()
                .flat_map(|p| p.iter())
                .any(|r| ring_crosses_antimeridian(r)),
        }
    }

    /// Normalize / split geometries that cross the antimeridian.
    ///
    /// Strategy (surgical, no full topology engine):
    /// 1. Shift longitudes into a continuous frame around the ring's first
    ///    finite vertex (unwrap ±360 jumps).
    /// 2. If the unwrapped bbox still spans ≤ 180°, shift the whole geometry
    ///    back into [-180, 180] and return a single part.
    /// 3. Otherwise split the **axis-aligned bbox** via
    ///    [`BBox::split_antimeridian`] and clip rings to each half with a
    ///    simple lon cut (Sutherland–style edge splits on meridians).
    ///
    /// Points / MultiPoints are lon-normalized only. Empty results are
    /// filtered out. Exact spherical clipping of complex polygons is deferred;
    /// this is enough for STRtree indexing and predicate pre-filters.
    pub fn split_antimeridian(&self) -> Vec<Geometry> {
        match self {
            Geometry::Point(p) => {
                let n = normalize_lon_coord(*p);
                if n.is_finite() {
                    vec![Geometry::Point(n)]
                } else {
                    Vec::new()
                }
            }
            Geometry::MultiPoint(pts) => {
                let pts: Vec<_> = pts.iter().copied().map(normalize_lon_coord).collect();
                if pts.is_empty() {
                    Vec::new()
                } else {
                    vec![Geometry::MultiPoint(pts)]
                }
            }
            Geometry::Polyline(parts) => split_parts_geometry(parts, true),
            Geometry::Polygon(rings) => split_parts_geometry(rings, false),
            Geometry::MultiPolygon(polys) => {
                let mut out = Vec::new();
                for poly in polys {
                    out.extend(split_parts_geometry(poly, false));
                }
                out
            }
        }
    }

    /// Serialize to a minimal OGC WKT string (lon lat order).
    ///
    /// Intended for Milvus `GEOMETRY` / PostGIS interchange used by the
    /// GIS `gis_adapter` path. Empty multi-geometries become
    /// `MULTIPOINT EMPTY` / `MULTILINESTRING EMPTY` / `MULTIPOLYGON EMPTY`.
    pub fn to_wkt(&self) -> String {
        match self {
            Geometry::Point(p) => format!("POINT ({} {})", p.lon, p.lat),
            Geometry::MultiPoint(pts) if pts.is_empty() => "MULTIPOINT EMPTY".into(),
            Geometry::MultiPoint(pts) => {
                let body = pts
                    .iter()
                    .map(|p| format!("({} {})", p.lon, p.lat))
                    .collect::<Vec<_>>()
                    .join(", ");
                format!("MULTIPOINT ({body})")
            }
            Geometry::Polyline(parts) if parts.is_empty() || parts.iter().all(|p| p.is_empty()) => {
                "MULTILINESTRING EMPTY".into()
            }
            Geometry::Polyline(parts) if parts.len() == 1 => {
                format!("LINESTRING ({})", format_ring_wkt(&parts[0]))
            }
            Geometry::Polyline(parts) => {
                let body = parts
                    .iter()
                    .map(|part| format!("({})", format_ring_wkt(part)))
                    .collect::<Vec<_>>()
                    .join(", ");
                format!("MULTILINESTRING ({body})")
            }
            Geometry::Polygon(rings) if rings.is_empty() || rings.iter().all(|r| r.is_empty()) => {
                "POLYGON EMPTY".into()
            }
            Geometry::Polygon(rings) => {
                let body = rings
                    .iter()
                    .map(|ring| format!("({})", format_ring_wkt(ring)))
                    .collect::<Vec<_>>()
                    .join(", ");
                format!("POLYGON ({body})")
            }
            Geometry::MultiPolygon(polys)
                if polys.is_empty()
                    || polys
                        .iter()
                        .all(|poly| poly.is_empty() || poly.iter().all(|r| r.is_empty())) =>
            {
                "MULTIPOLYGON EMPTY".into()
            }
            Geometry::MultiPolygon(polys) => {
                let body = polys
                    .iter()
                    .map(|poly| {
                        let rings = poly
                            .iter()
                            .map(|ring| format!("({})", format_ring_wkt(ring)))
                            .collect::<Vec<_>>()
                            .join(", ");
                        format!("({rings})")
                    })
                    .collect::<Vec<_>>()
                    .join(", ");
                format!("MULTIPOLYGON ({body})")
            }
        }
    }
}

fn format_ring_wkt(ring: &[Coordinate]) -> String {
    ring.iter()
        .map(|p| format!("{} {}", p.lon, p.lat))
        .collect::<Vec<_>>()
        .join(", ")
}

fn ring_crosses_antimeridian(ring: &[Coordinate]) -> bool {
    let finite: Vec<_> = ring.iter().filter(|p| p.is_finite()).collect();
    if finite.len() < 2 {
        return false;
    }
    for w in finite.windows(2) {
        if (w[0].lon - w[1].lon).abs() > 180.0 {
            return true;
        }
    }
    false
}

fn normalize_lon(lon: f64) -> f64 {
    if !lon.is_finite() {
        return lon;
    }
    let mut x = ((lon + 180.0) % 360.0 + 360.0) % 360.0 - 180.0;
    // Map 180 → -180 for a stable half-open feel at the cut.
    if x == 180.0 {
        x = -180.0;
    }
    x
}

fn normalize_lon_coord(p: Coordinate) -> Coordinate {
    Coordinate::new(normalize_lon(p.lon), p.lat)
}

/// Unwrap consecutive longitudes so jumps stay within ±180° of the previous.
fn unwrap_ring(ring: &[Coordinate]) -> Vec<Coordinate> {
    let mut out = Vec::with_capacity(ring.len());
    let mut prev_lon: Option<f64> = None;
    for p in ring {
        if !p.is_finite() {
            continue;
        }
        let lon = match prev_lon {
            None => p.lon,
            Some(prev) => {
                let mut lon = p.lon;
                while lon - prev > 180.0 {
                    lon -= 360.0;
                }
                while lon - prev < -180.0 {
                    lon += 360.0;
                }
                lon
            }
        };
        prev_lon = Some(lon);
        out.push(Coordinate::new(lon, p.lat));
    }
    out
}

fn shift_ring_to_lon_range(ring: &[Coordinate]) -> Vec<Coordinate> {
    if ring.is_empty() {
        return Vec::new();
    }
    let mut min_lon = f64::INFINITY;
    for p in ring {
        min_lon = min_lon.min(p.lon);
    }
    // Shift so the westernmost vertex lands in [-180, 180].
    let shift = if min_lon < -180.0 {
        (1.0 + ((-180.0 - min_lon) / 360.0).floor()) * 360.0
    } else if min_lon > 180.0 {
        -((min_lon - 180.0) / 360.0).ceil() * 360.0
    } else {
        0.0
    };
    ring.iter()
        .map(|p| Coordinate::new(normalize_lon(p.lon + shift), p.lat))
        .collect()
}

fn split_parts_geometry(parts: &[Vec<Coordinate>], as_polyline: bool) -> Vec<Geometry> {
    if parts.is_empty() {
        return Vec::new();
    }
    let unwrapped: Vec<Vec<Coordinate>> = parts.iter().map(|r| unwrap_ring(r)).collect();
    if unwrapped.iter().all(|r| r.is_empty()) {
        return Vec::new();
    }

    let mut bbox = BBox::empty();
    for ring in &unwrapped {
        for p in ring {
            bbox.expand_to_include(p);
        }
    }
    if !bbox.is_finite() {
        return Vec::new();
    }

    // Fits in a ≤180° frame after unwrap → try a single normalized geometry.
    // If re-wrapping into [-180, 180] recreates a dateline jump (short arc
    // across ±180), fall through to the clip path instead.
    if bbox.width() <= 180.0 {
        let shifted: Vec<Vec<Coordinate>> = unwrapped
            .iter()
            .map(|r| shift_ring_to_lon_range(r))
            .collect();
        let still_crosses = shifted.iter().any(|r| ring_crosses_antimeridian(r));
        if !still_crosses {
            return if as_polyline {
                vec![Geometry::Polyline(shifted)]
            } else {
                vec![Geometry::Polygon(shifted)]
            };
        }
        // Fall through: clip the normalized rings at the dateline.
        let mut west_parts = Vec::new();
        let mut east_parts = Vec::new();
        for ring in &shifted {
            let (w, e) = clip_ring_at_dateline(ring);
            if w.len() >= 2 {
                west_parts.push(w);
            }
            if e.len() >= 2 {
                east_parts.push(e);
            }
        }
        let mut out = Vec::new();
        if !west_parts.is_empty() {
            out.push(if as_polyline {
                Geometry::Polyline(west_parts)
            } else {
                Geometry::Polygon(west_parts)
            });
        }
        if !east_parts.is_empty() {
            out.push(if as_polyline {
                Geometry::Polyline(east_parts)
            } else {
                Geometry::Polygon(east_parts)
            });
        }
        if !out.is_empty() {
            return out;
        }
    }

    // Wide span (or failed short-arc normalize): project and clip at dateline.
    let mut west_parts = Vec::new();
    let mut east_parts = Vec::new();
    for ring in &unwrapped {
        let normalized: Vec<Coordinate> = ring
            .iter()
            .map(|p| Coordinate::new(normalize_lon(p.lon), p.lat))
            .collect();
        if ring_crosses_antimeridian(&normalized) || spans_dateline_bbox(&normalized) {
            let (w, e) = clip_ring_at_dateline(&normalized);
            if w.len() >= 2 {
                west_parts.push(w);
            }
            if e.len() >= 2 {
                east_parts.push(e);
            }
        } else if let Some(p0) = normalized.first() {
            if p0.lon >= 0.0 {
                west_parts.push(normalized);
            } else {
                east_parts.push(normalized);
            }
        }
    }

    let mut out = Vec::new();
    if !west_parts.is_empty() {
        out.push(if as_polyline {
            Geometry::Polyline(west_parts)
        } else {
            Geometry::Polygon(west_parts)
        });
    }
    if !east_parts.is_empty() {
        out.push(if as_polyline {
            Geometry::Polyline(east_parts)
        } else {
            Geometry::Polygon(east_parts)
        });
    }
    out
}

fn spans_dateline_bbox(ring: &[Coordinate]) -> bool {
    let mut min_lon = f64::INFINITY;
    let mut max_lon = f64::NEG_INFINITY;
    for p in ring {
        if p.is_finite() {
            min_lon = min_lon.min(p.lon);
            max_lon = max_lon.max(p.lon);
        }
    }
    max_lon - min_lon > 180.0
}

/// Clip a ring that may jump across ±180 into west (≥0) and east (<0) pieces.
///
/// Inserts interpolated vertices on the ±180 meridians. Not a full polygon
/// clipper — sufficient for bbox/index pre-filters and simple SAR footprints.
fn clip_ring_at_dateline(ring: &[Coordinate]) -> (Vec<Coordinate>, Vec<Coordinate>) {
    if ring.len() < 2 {
        return (Vec::new(), Vec::new());
    }
    let mut west = Vec::new();
    let mut east = Vec::new();

    for i in 0..ring.len() - 1 {
        let a = ring[i];
        let b = ring[i + 1];
        if !a.is_finite() || !b.is_finite() {
            continue;
        }
        push_side(&mut west, &mut east, a);
        if (a.lon - b.lon).abs() > 180.0 {
            let cut_a = if a.lon > 0.0 { 180.0 } else { -180.0 };
            let cut_b = -cut_a;
            let mut b_lon = b.lon;
            while b_lon - a.lon > 180.0 {
                b_lon -= 360.0;
            }
            while b_lon - a.lon < -180.0 {
                b_lon += 360.0;
            }
            let denom = b_lon - a.lon;
            if denom.abs() > f64::EPSILON {
                let t = ((cut_a - a.lon) / denom).clamp(0.0, 1.0);
                let lat = a.lat + t * (b.lat - a.lat);
                push_side(&mut west, &mut east, Coordinate::new(cut_a, lat));
                push_side(&mut west, &mut east, Coordinate::new(cut_b, lat));
            }
        }
    }
    if let Some(&last) = ring.last() {
        if last.is_finite() {
            push_side(&mut west, &mut east, last);
        }
    }
    (west, east)
}

fn push_side(west: &mut Vec<Coordinate>, east: &mut Vec<Coordinate>, p: Coordinate) {
    if p.lon >= 0.0 {
        west.push(p);
    } else {
        east.push(p);
    }
}

/// A geographic feature with optional identifier, geometry, and attribute properties.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GeoFeature {
    /// Optional feature identifier.
    pub id: Option<String>,
    /// The geometry of this feature.
    pub geometry: Geometry,
    /// Arbitrary attribute bag (mirrors GeoJSON `properties`).
    pub properties: serde_json::Value,
}

impl GeoFeature {
    /// Creates a new geographic feature.
    pub fn new(id: Option<String>, geometry: Geometry, properties: serde_json::Value) -> Self {
        GeoFeature {
            id,
            geometry,
            properties,
        }
    }

    /// Creates a new feature with empty properties.
    pub fn with_geometry(geometry: Geometry) -> Self {
        GeoFeature {
            id: None,
            geometry,
            properties: serde_json::json!({}),
        }
    }

    /// Returns the bounding box of this feature's geometry.
    pub fn bbox(&self) -> Option<BBox> {
        self.geometry.bbox()
    }

    /// Returns a reference to a property value by key.
    pub fn get_property(&self, key: &str) -> Option<&serde_json::Value> {
        self.properties.get(key)
    }

    /// Sets a property value.
    pub fn set_property(&mut self, key: impl Into<String>, value: impl Into<serde_json::Value>) {
        if let Some(obj) = self.properties.as_object_mut() {
            obj.insert(key.into(), value.into());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_coordinate_distance() {
        // Distance between Brasília and São Paulo (approx 870 km)
        let brasilia = Coordinate::new(-47.9297, -15.7801);
        let sao_paulo = Coordinate::new(-46.6333, -23.5505);
        let dist = brasilia.distance_km(&sao_paulo);
        assert!(dist > 850.0 && dist < 900.0, "Distance was {}", dist);
    }

    #[test]
    fn test_coordinate_nan_is_invalid() {
        // WHY: NaN must fail validation — a bare range check can miss it and
        // poison downstream bbox / distance math.
        let bad = Coordinate::new(f64::NAN, 0.0);
        assert!(!bad.is_finite());
        assert!(!bad.is_valid());
        assert!(bad.validate().is_err());
        assert!(Coordinate::new(0.0, 0.0).distance_km(&bad).is_nan());
    }

    #[test]
    fn test_bbox_intersection() {
        let a = BBox::new(0.0, 0.0, 10.0, 10.0);
        let b = BBox::new(5.0, 5.0, 15.0, 15.0);
        let intersect = a.intersection(&b).unwrap();
        assert_eq!(intersect.min_lon, 5.0);
        assert_eq!(intersect.max_lat, 10.0);
    }

    #[test]
    fn test_bbox_no_intersection() {
        let a = BBox::new(0.0, 0.0, 10.0, 10.0);
        let b = BBox::new(20.0, 20.0, 30.0, 30.0);
        assert!(a.intersection(&b).is_none());
    }

    #[test]
    fn test_bbox_contains() {
        let outer = BBox::new(0.0, 0.0, 10.0, 10.0);
        let inner = BBox::new(2.0, 2.0, 8.0, 8.0);
        assert!(outer.contains(&inner));
        assert!(!inner.contains(&outer));
    }

    #[test]
    fn test_bbox_contains_point() {
        let bbox = BBox::new(0.0, 0.0, 10.0, 10.0);
        assert!(bbox.contains_point(&Coordinate::new(5.0, 5.0)));
        // Inclusive edges: corners belong to the box.
        assert!(bbox.contains_point(&Coordinate::new(0.0, 0.0)));
        assert!(bbox.contains_point(&Coordinate::new(10.0, 10.0)));
        assert!(!bbox.contains_point(&Coordinate::new(15.0, 5.0)));
        assert!(!bbox.contains_point(&Coordinate::new(f64::NAN, 5.0)));
    }

    #[test]
    fn test_bbox_touching_edges_intersect() {
        // WHY: closed-edge semantics — boxes that share only a boundary must
        // still count as intersecting for tile / feature overlap filters.
        let a = BBox::new(0.0, 0.0, 10.0, 10.0);
        let b = BBox::new(10.0, 0.0, 20.0, 10.0);
        assert!(a.intersects(&b));
        let inter = a.intersection(&b).unwrap();
        assert_eq!(inter.area(), 0.0); // shared edge is degenerate
        assert_eq!(a.iou(&b), 0.0);
    }

    #[test]
    fn test_bbox_iou_partial_overlap() {
        let a = BBox::new(0.0, 0.0, 10.0, 10.0); // area 100
        let b = BBox::new(5.0, 5.0, 15.0, 15.0); // area 100, inter 25
        let iou = a.iou(&b);
        assert!((iou - 25.0 / 175.0).abs() < 1e-12, "iou={iou}");
    }

    #[test]
    fn test_bbox_iou_nan_and_degenerate() {
        let a = BBox::new(0.0, 0.0, 10.0, 10.0);
        let nan = BBox::new(f64::NAN, 0.0, 1.0, 1.0);
        assert_eq!(a.iou(&nan), 0.0);
        assert!(!nan.intersects(&a));
        let point = BBox::new(1.0, 1.0, 1.0, 1.0);
        assert!(point.is_degenerate());
        assert_eq!(point.area(), 0.0);
        assert_eq!(a.iou(&point), 0.0);
    }

    #[test]
    fn test_bbox_expand_ignores_nan() {
        let mut bbox = BBox::empty();
        bbox.expand_to_include(&Coordinate::new(1.0, 2.0));
        bbox.expand_to_include(&Coordinate::new(f64::NAN, 99.0));
        assert_eq!(bbox, BBox::new(1.0, 2.0, 1.0, 2.0));
    }

    #[test]
    fn test_bbox_wms_string() {
        let bbox = BBox::new(-48.0, -16.0, -47.0, -15.0);
        let s = bbox.to_wms_string();
        assert_eq!(s, "-48,-16,-47,-15");

        let parsed = BBox::from_wms_string(&s).unwrap();
        assert_eq!(parsed.min_lon, -48.0);
        assert_eq!(parsed.max_lat, -15.0);

        assert!(BBox::from_wms_string("1,2,3").is_err());
        assert!(BBox::from_wms_string("1,2,3,nan").is_err());
        assert!(BBox::from_wms_string(" 1 , 2 , 3 , 4 ").is_ok());
    }

    #[test]
    fn test_bbox_normalize() {
        let mut bbox = BBox::new(10.0, 10.0, 0.0, 0.0);
        bbox.normalize();
        assert_eq!(bbox.min_lon, 0.0);
        assert_eq!(bbox.max_lon, 10.0);
        assert_eq!(bbox.min_lat, 0.0);
        assert_eq!(bbox.max_lat, 10.0);
    }

    #[test]
    fn test_geometry_bbox() {
        let geom = Geometry::Polygon(vec![vec![
            Coordinate::new(0.0, 0.0),
            Coordinate::new(10.0, 0.0),
            Coordinate::new(10.0, 10.0),
            Coordinate::new(0.0, 10.0),
            Coordinate::new(0.0, 0.0),
        ]]);
        let bbox = geom.bbox().unwrap();
        assert_eq!(bbox.min_lon, 0.0);
        assert_eq!(bbox.max_lon, 10.0);
        assert_eq!(bbox.min_lat, 0.0);
        assert_eq!(bbox.max_lat, 10.0);
    }

    #[test]
    fn test_geometry_bbox_skips_nan() {
        // WHY: a single NaN vertex must not collapse the AABB to NaN and
        // break spatial filters for otherwise-valid rings.
        let geom = Geometry::MultiPoint(vec![
            Coordinate::new(f64::NAN, 0.0),
            Coordinate::new(1.0, 2.0),
            Coordinate::new(3.0, 4.0),
        ]);
        let bbox = geom.bbox().unwrap();
        assert_eq!(bbox, BBox::new(1.0, 2.0, 3.0, 4.0));
    }

    #[test]
    fn test_geometry_to_wkt_point_and_polygon() {
        // WHY: GIS gis_adapter stores Milvus GEOMETRY as WKT; the Rust
        // core must emit the same lon/lat order without inventing Z/M.
        let pt = Geometry::Point(Coordinate::new(-47.9, -15.8));
        assert_eq!(pt.to_wkt(), "POINT (-47.9 -15.8)");

        let poly = Geometry::Polygon(vec![vec![
            Coordinate::new(0.0, 0.0),
            Coordinate::new(1.0, 0.0),
            Coordinate::new(1.0, 1.0),
            Coordinate::new(0.0, 0.0),
        ]]);
        assert_eq!(poly.to_wkt(), "POLYGON ((0 0, 1 0, 1 1, 0 0))");
    }

    #[test]
    fn test_multipolygon_preserves_groups() {
        let geom = Geometry::MultiPolygon(vec![
            vec![vec![
                Coordinate::new(0.0, 0.0),
                Coordinate::new(1.0, 0.0),
                Coordinate::new(1.0, 1.0),
                Coordinate::new(0.0, 0.0),
            ]],
            vec![vec![
                Coordinate::new(10.0, 10.0),
                Coordinate::new(11.0, 10.0),
                Coordinate::new(11.0, 11.0),
                Coordinate::new(10.0, 10.0),
            ]],
        ]);
        assert_eq!(geom.geometry_type(), "MultiPolygon");
        assert_eq!(geom.coordinate_count(), 8);
        let bbox = geom.bbox().unwrap();
        assert_eq!(bbox.min_lon, 0.0);
        assert_eq!(bbox.max_lon, 11.0);
        assert!(geom.to_wkt().starts_with("MULTIPOLYGON"));
    }

    #[test]
    fn test_bbox_split_antimeridian() {
        // WHY: dateline-crossing extents must become two non-wrapping halves
        // before STRtree / IoU predicates (GIS Pacific sector footprints).
        let wrap = BBox::new(170.0, -10.0, -170.0, 10.0);
        assert!(wrap.crosses_antimeridian());
        let parts = wrap.split_antimeridian();
        assert_eq!(parts.len(), 2);
        assert_eq!(parts[0], BBox::new(170.0, -10.0, 180.0, 10.0));
        assert_eq!(parts[1], BBox::new(-180.0, -10.0, -170.0, 10.0));
        assert!(wrap.intersects_antimeridian_aware(&BBox::new(175.0, -1.0, 176.0, 1.0)));
        assert!(wrap.intersects_antimeridian_aware(&BBox::new(-176.0, -1.0, -175.0, 1.0)));
        assert!(wrap.contains_point_antimeridian_aware(&Coordinate::new(175.0, 0.0)));
        assert!(wrap.contains_point_antimeridian_aware(&Coordinate::new(-175.0, 0.0)));
        assert!(!wrap.contains_point_antimeridian_aware(&Coordinate::new(0.0, 0.0)));
    }

    #[test]
    fn test_geometry_split_antimeridian_line() {
        let line = Geometry::Polyline(vec![vec![
            Coordinate::new(170.0, 0.0),
            Coordinate::new(-170.0, 0.0),
        ]]);
        assert!(line.crosses_antimeridian());
        let parts = line.split_antimeridian();
        assert!(
            parts.len() >= 2,
            "expected split, got {} parts: {parts:?}",
            parts.len()
        );
        for part in &parts {
            assert!(!part.crosses_antimeridian());
        }
    }
}

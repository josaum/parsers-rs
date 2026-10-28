//! Spherical (WGS84 mean-radius) geodesic helpers for geographic CRS.
//!
//! # Assumptions
//!
//! - Coordinates are **EPSG:4326** lon/lat in decimal degrees.
//! - The Earth is modeled as a **sphere** with mean radius
//!   [`EARTH_RADIUS_M`] (6_371_008.8 m) — good enough for relative overlap /
//!   IoU scoring and GIS sector areas; not a full ellipsoidal (Karney)
//!   geodesic. Documented deliberately so callers do not confuse this with
//!   PROJ / GeographicLib accuracy.
//! - Bounding-box geodesic area treats the box as a spherical quadrangle
//!   bounded by meridians and parallels (exact on a sphere).
//! - Polygon geodesic area uses the **spherical excess** (Girard's theorem)
//!   via the trapezoidal longitude-difference formula.

use crate::types::{BBox, Coordinate, Geometry};

/// WGS84 authalic / mean Earth radius in metres.
pub const EARTH_RADIUS_M: f64 = 6_371_008.8;

/// Square metres per square kilometre.
const M2_PER_KM2: f64 = 1_000_000.0;

/// Approximate geodesic area of a lon-lat bounding box in square metres.
///
/// Exact for a spherical Earth:
/// `R² * Δλ * (sin φ₂ − sin φ₁)` with longitudes in radians.
///
/// Returns `0.0` for non-finite, inverted, or antimeridian-crossing boxes
/// (split first with [`BBox::split_antimeridian`]).
pub fn bbox_area_m2(bbox: &BBox) -> f64 {
    if !bbox.is_finite() || !bbox.is_ordered() || bbox.crosses_antimeridian() {
        return 0.0;
    }
    let d_lon = (bbox.max_lon - bbox.min_lon).to_radians();
    if d_lon <= 0.0 {
        return 0.0;
    }
    let sin_max = bbox.max_lat.to_radians().sin();
    let sin_min = bbox.min_lat.to_radians().sin();
    let area = EARTH_RADIUS_M * EARTH_RADIUS_M * d_lon * (sin_max - sin_min);
    if area.is_finite() && area > 0.0 {
        area
    } else {
        0.0
    }
}

/// Geodesic area of a bbox in square kilometres.
pub fn bbox_area_km2(bbox: &BBox) -> f64 {
    bbox_area_m2(bbox) / M2_PER_KM2
}

/// Intersection-over-union using spherical bbox areas (m²).
///
/// Antimeridian-crossing inputs are split; intersection area is the sum of
/// pairwise spherical intersections of the split parts. Returns `0.0` when
/// either side has zero geodesic area.
pub fn bbox_iou_geodesic(a: &BBox, b: &BBox) -> f64 {
    let a_parts = a.split_antimeridian();
    let b_parts = b.split_antimeridian();

    let mut area_a = 0.0;
    for p in &a_parts {
        area_a += bbox_area_m2(p);
    }
    let mut area_b = 0.0;
    for p in &b_parts {
        area_b += bbox_area_m2(p);
    }
    if area_a <= 0.0 || area_b <= 0.0 {
        return 0.0;
    }

    let mut inter = 0.0;
    for pa in &a_parts {
        for pb in &b_parts {
            if let Some(i) = pa.intersection(pb) {
                inter += bbox_area_m2(&i);
            }
        }
    }
    if inter <= 0.0 {
        return 0.0;
    }
    inter / (area_a + area_b - inter)
}

/// Signed spherical excess area of a closed ring in square metres.
///
/// Ring may be open or closed (first==last). Absolute value is the area;
/// sign follows ring orientation (positive ≈ CCW on a north-up map near the
/// equator). Returns `0.0` for rings with fewer than 3 distinct vertices or
/// non-finite coordinates.
pub fn ring_area_m2(ring: &[Coordinate]) -> f64 {
    let pts: Vec<&Coordinate> = ring.iter().filter(|p| p.is_finite()).collect();
    if pts.len() < 3 {
        return 0.0;
    }

    // Drop duplicate closing vertex if present.
    let n = if pts.len() > 1
        && (pts[0].lon - pts[pts.len() - 1].lon).abs() < 1e-15
        && (pts[0].lat - pts[pts.len() - 1].lat).abs() < 1e-15
    {
        pts.len() - 1
    } else {
        pts.len()
    };
    if n < 3 {
        return 0.0;
    }

    // Spherical excess via Σ Δλ * (sin φ_i + sin φ_{i+1}) / 2  (authalic).
    let mut total = 0.0;
    for i in 0..n {
        let p1 = pts[i];
        let p2 = pts[(i + 1) % n];
        let mut d_lon = (p2.lon - p1.lon).to_radians();
        // Unwrap short arc across antimeridian for consecutive edges.
        if d_lon > std::f64::consts::PI {
            d_lon -= 2.0 * std::f64::consts::PI;
        } else if d_lon < -std::f64::consts::PI {
            d_lon += 2.0 * std::f64::consts::PI;
        }
        total += d_lon * (p1.lat.to_radians().sin() + p2.lat.to_radians().sin());
    }
    let area = total * EARTH_RADIUS_M * EARTH_RADIUS_M * 0.5;
    if area.is_finite() {
        area
    } else {
        0.0
    }
}

/// Absolute geodesic area of a polygon (exterior minus holes) in m².
pub fn polygon_area_m2(rings: &[Vec<Coordinate>]) -> f64 {
    if rings.is_empty() {
        return 0.0;
    }
    let exterior = ring_area_m2(&rings[0]).abs();
    let holes: f64 = rings[1..].iter().map(|r| ring_area_m2(r).abs()).sum();
    (exterior - holes).max(0.0)
}

/// Absolute geodesic area for supported geometry variants (m²).
///
/// Points / polylines return `0.0`. MultiPolygon sums component areas.
pub fn geometry_area_m2(geom: &Geometry) -> f64 {
    match geom {
        Geometry::Point(_) | Geometry::MultiPoint(_) | Geometry::Polyline(_) => 0.0,
        Geometry::Polygon(rings) => polygon_area_m2(rings),
        Geometry::MultiPolygon(polys) => polys.iter().map(|p| polygon_area_m2(p)).sum(),
    }
}

/// Absolute geodesic area in km².
pub fn geometry_area_km2(geom: &Geometry) -> f64 {
    geometry_area_m2(geom) / M2_PER_KM2
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn equator_one_degree_square_area() {
        // WHY: 1°×1° at the equator ≈ 111.2 km × 111.2 km ≈ 12_364 km² on a
        // sphere — anchors the spherical bbox formula against a known scale.
        let bbox = BBox::new(0.0, 0.0, 1.0, 1.0);
        let km2 = bbox_area_km2(&bbox);
        assert!((km2 - 12_364.0).abs() < 50.0, "equator 1° box km2={km2}");
    }

    #[test]
    fn high_latitude_box_smaller_than_equator() {
        let eq = bbox_area_m2(&BBox::new(0.0, 0.0, 1.0, 1.0));
        let polar = bbox_area_m2(&BBox::new(0.0, 70.0, 1.0, 71.0));
        assert!(polar < eq * 0.5, "polar={polar} eq={eq}");
    }

    #[test]
    fn geodesic_iou_identical_is_one() {
        let a = BBox::new(-48.0, -16.0, -47.0, -15.0);
        assert!((bbox_iou_geodesic(&a, &a) - 1.0).abs() < 1e-12);
    }

    #[test]
    fn geodesic_iou_disjoint_is_zero() {
        let a = BBox::new(0.0, 0.0, 1.0, 1.0);
        let b = BBox::new(10.0, 10.0, 11.0, 11.0);
        assert_eq!(bbox_iou_geodesic(&a, &b), 0.0);
    }

    #[test]
    fn unit_square_polygon_area_matches_bbox() {
        let ring = vec![
            Coordinate::new(0.0, 0.0),
            Coordinate::new(1.0, 0.0),
            Coordinate::new(1.0, 1.0),
            Coordinate::new(0.0, 1.0),
            Coordinate::new(0.0, 0.0),
        ];
        let poly = Geometry::Polygon(vec![ring]);
        let poly_a = geometry_area_m2(&poly);
        let box_a = bbox_area_m2(&BBox::new(0.0, 0.0, 1.0, 1.0));
        // Spherical excess vs spherical quadrangle should agree within ~1%.
        assert!(
            (poly_a - box_a).abs() / box_a < 0.02,
            "poly={poly_a} box={box_a}"
        );
    }

    #[test]
    fn antimeridian_bbox_area_requires_split() {
        let wrap = BBox::new(170.0, -5.0, -170.0, 5.0);
        assert_eq!(bbox_area_m2(&wrap), 0.0);
        let parts = wrap.split_antimeridian();
        let sum: f64 = parts.iter().map(bbox_area_m2).sum();
        assert!(sum > 0.0);
    }
}

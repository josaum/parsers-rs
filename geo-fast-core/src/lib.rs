//! `geo-fast-core` — high-performance GIS format parser and spatial index.
//!
//! # Features
//! - **`http`** (optional): enables [`wms`] and [`wfs`] async HTTP clients.
//!
//! # Coordinate system
//! Shared types ([`Coordinate`], [`BBox`], [`Geometry`]) use **EPSG:4326**
//! with axis order **(longitude, latitude)** — GeoJSON / WMS convention.
//! Bounding-box predicates use **closed / inclusive** edges.
//!
//! Antimeridian-crossing extents: use [`BBox::split_antimeridian`] /
//! [`Geometry::split_antimeridian`] before predicates or STRtree queries.
//! Geodesic area / IoU helpers live in [`geodesic`] (spherical WGS84 mean
//! radius — not a full PROJ / ellipsoidal stack).
//!
//! # Supported formats
//! | Format          | Module              | Read | Write |
//! |-----------------|---------------------|------|-------|
//! | GeoTIFF         | [`geotiff`]         | ✓    | –     |
//! | Shapefile       | [`shapefile`]       | ✓¹   | –     |
//! | GeoJSON         | [`geojson`]         | ✓    | ✓     |
//! | WMS (OGC)       | [`wms`]             | ✓    | –     |
//! | WFS (OGC)       | [`wfs`]             | ✓    | –     |
//! | SAR (SAFE/CEOS) | [`sar`]             | ✓    | –     |
//! | Thermal (FLIR)  | [`thermal`]         | ✓    | –     |
//! | Hyperspectral   | [`hyperspectral`]   | ✓    | –     |
//!
//! ¹ XY + Z/M shape types; Z/M attached as `properties["_z"]` / `["_m"]`.
//!
//! # Spatial index
//! [`index::SpatialIndex`] is an STR-packed R-tree over feature MBRs.
//!
//! # Deferred (documented)
//! - GeoTIFF **write** and a full PROJ CRS transform stack — read-path EPSG
//!   extraction via GeoKeys remains in [`geotiff`]; no write / reprojection.
//! - ENVI **complex** pixel types (`ComplexF32` / `ComplexF64`) — integer and
//!   float DN paths (including I64/U64) are supported; complex magnitude/phase
//!   policy is not defined for the f32 band path and fails loud.
//!
//! # Quick Start
//! ```
//! use geo_fast_core::{GeoFeature, Geometry, GeoPoint, parse_geojson_str};
//!
//! // Parse GeoJSON
//! let geojson = r#"{"type":"Feature","geometry":{"type":"Point","coordinates":[1.0,2.0]},"properties":{}}"#;
//! let features = parse_geojson_str(geojson).unwrap();
//! assert_eq!(features[0].geometry.to_wkt(), "POINT (1 2)");
//! ```

#![deny(missing_docs)]
#![warn(rust_2018_idioms)]

pub mod error;
pub mod geodesic;
pub mod geojson;
pub mod geotiff;
pub mod hyperspectral;
pub mod index;
pub mod sar;
pub mod shapefile;
pub mod thermal;
pub mod types;

#[cfg(feature = "http")]
pub mod wms;

#[cfg(feature = "http")]
pub mod wfs;

// Re-export the most commonly used items at crate root.
pub use error::{GeoError, Result};
pub use geodesic::{
    bbox_area_km2, bbox_area_m2, bbox_iou_geodesic, geometry_area_km2, geometry_area_m2,
    polygon_area_m2, ring_area_m2, EARTH_RADIUS_M,
};
pub use index::SpatialIndex;
pub use types::{BBox, Coordinate, GeoFeature, GeoPoint, Geometry};

// Re-export parser functions for convenience
pub use geojson::{features_to_geojson, geojson_to_features, parse_geojson_str};
pub use geotiff::{parse_geotiff_bands, parse_geotiff_metadata, GeoTiffMetadata};
pub use shapefile::{parse_shapefile, parse_shapefile_bytes, ShapefileData};

/// Prelude module for convenient imports.
///
/// Use `use geo_fast_core::prelude::*;` to import common types.
pub mod prelude {
    pub use crate::error::{GeoError, Result};
    pub use crate::index::SpatialIndex;
    pub use crate::types::{BBox, Coordinate, GeoFeature, GeoPoint, Geometry};
    pub use crate::{
        bbox_iou_geodesic, parse_geojson_str, parse_geotiff_metadata, parse_shapefile,
    };
}

/// Crate version.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

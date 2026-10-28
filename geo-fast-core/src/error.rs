//! Error types for `geo-fast-core`.

use std::fmt;

/// Result type alias using [`GeoError`].
pub type Result<T> = std::result::Result<T, GeoError>;

/// Error types for geo-fast-core operations.
#[derive(Debug)]
pub enum GeoError {
    /// I/O error (file system, network, etc.).
    Io(std::io::Error),

    /// TIFF decode error.
    Tiff(String),

    /// Invalid format or malformed data.
    InvalidFormat(String),

    /// Unsupported format or feature.
    UnsupportedFormat(String),

    /// HTTP error (only available with `http` feature).
    #[cfg(feature = "http")]
    Http(String),

    /// XML parse error.
    Xml(String),

    /// JSON parse error.
    Json(serde_json::Error),

    /// Coordinate system / projection error.
    Projection(String),

    /// Validation error.
    Validation(String),
}

impl fmt::Display for GeoError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            GeoError::Io(e) => write!(f, "I/O error: {e}"),
            GeoError::Tiff(msg) => write!(f, "TIFF decode error: {msg}"),
            GeoError::InvalidFormat(msg) => write!(f, "Invalid format: {msg}"),
            GeoError::UnsupportedFormat(msg) => write!(f, "Unsupported format: {msg}"),
            #[cfg(feature = "http")]
            GeoError::Http(msg) => write!(f, "HTTP error: {msg}"),
            GeoError::Xml(msg) => write!(f, "XML parse error: {msg}"),
            GeoError::Json(e) => write!(f, "JSON parse error: {e}"),
            GeoError::Projection(msg) => write!(f, "Projection error: {msg}"),
            GeoError::Validation(msg) => write!(f, "Validation error: {msg}"),
        }
    }
}

impl std::error::Error for GeoError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            GeoError::Io(e) => Some(e),
            GeoError::Json(e) => Some(e),
            _ => None,
        }
    }
}

impl From<std::io::Error> for GeoError {
    fn from(e: std::io::Error) -> Self {
        GeoError::Io(e)
    }
}

impl From<serde_json::Error> for GeoError {
    fn from(e: serde_json::Error) -> Self {
        GeoError::Json(e)
    }
}

impl From<tiff::TiffError> for GeoError {
    fn from(e: tiff::TiffError) -> Self {
        GeoError::Tiff(e.to_string())
    }
}

impl From<std::str::Utf8Error> for GeoError {
    fn from(e: std::str::Utf8Error) -> Self {
        GeoError::InvalidFormat(format!("Invalid UTF-8: {e}"))
    }
}

impl From<std::string::FromUtf8Error> for GeoError {
    fn from(e: std::string::FromUtf8Error) -> Self {
        GeoError::InvalidFormat(format!("Invalid UTF-8: {e}"))
    }
}

#[cfg(feature = "http")]
impl From<reqwest::Error> for GeoError {
    fn from(e: reqwest::Error) -> Self {
        GeoError::Http(e.to_string())
    }
}

#![deny(missing_docs)]
#![warn(rust_2018_idioms)]

//! # xsd-fast-core
//!
//! High-performance XSD schema parser with Arrow schema derivation and simple-type validation.
//!
//! Parses domain-neutral XSD schema sets for deterministic introspection and validation.
//!
//! ## What it covers
//!
//! - Parses `xs:element`, `xs:complexType`, `xs:simpleType`, `xs:sequence`, `xs:choice`,
//!   `xs:all`, `xs:extension`, `xs:restriction`, `xs:enumeration`, `xs:include`, `xs:import`
//! - Resolves multi-file schemas (relative `schemaLocation` paths)
//! - Derives an Arrow [`Schema`] from a root element (for downstream Flight / IPC)
//! - Validates XML byte slices against the parsed schema rules (enumeration, pattern,
//!   minLength/maxLength, minInclusive/maxInclusive for simple types)
//! - Produces structured [`ValidationError`] values — never panics on bad input
//!
//! ## Out of scope (v1)
//!
//! - `xs:key` / `xs:keyref` / `xs:unique` identity constraints
//! - `xs:union` member resolution beyond string fallback
//! - XPath-based assertions
//!
//! ## Quick Start
//!
//! ```rust,no_run
//! use xsd_fast_core::{SchemaSet, ParseOptions, ValidateOptions};
//!
//! // Load schema (follows xs:include / xs:import automatically)
//! let schema = SchemaSet::load("schemas/example.xsd", ParseOptions::default())?;
//!
//! // Validate an XML document
//! let xml = std::fs::read("document.xml")?;
//! let errors = schema.validate(&xml, ValidateOptions::default())?;
//! if errors.is_empty() {
//!     println!("valid");
//! }
//!
//! // Derive Arrow schema for a root element
//! #[cfg(feature = "arrow")]
//! {
//!     use xsd_fast_core::arrow::ArrowOptions;
//!     let arrow_schema = schema.to_arrow_schema("example", ArrowOptions::default())?;
//!     println!("{arrow_schema:?}");
//! }
//! # Ok::<(), anyhow::Error>(())
//! ```

pub mod parser;
pub mod schema;
pub mod validator;

#[cfg(feature = "arrow")]
pub mod arrow;

pub use parser::{ParseOptions, SchemaSet};
pub use schema::{
    ComplexType, ContentModel, Derivation, ElementDecl, EnumValue, OccurrenceConstraint,
    SchemaType, SimpleTypeKind, XsdType,
};
pub use validator::{ValidateOptions, ValidationError, ValidationSeverity};

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
//! - Follows chains of named simple types; `xs:list` keeps its item type and
//!   `xs:union` the kind its members share (a string when they differ)
//! - Resolves `xs:attribute ref` and `xs:attributeGroup ref`, nested groups
//!   included
//! - Models `xs:any` element wildcards for validation; they derive no column
//!
//! ## What the default Arrow shape leaves out
//!
//! An attribute that reaches a type through `xs:attribute ref` or
//! `xs:attributeGroup ref` is not a column of a child element whose own
//! content derives exactly one column (typically its text): that child stays
//! the one column, named after the element, as before references were
//! resolved. The same type keeps those attributes as the root element, or as
//! a child with no column or several of its own.
//! `arrow::ArrowShape::attributes_on_simple_children` makes such a child a
//! struct of its value and all its attributes.
//! - Resolves multi-file schemas (relative `schemaLocation` paths)
//! - Derives an Arrow `Schema` from a root element (for downstream Flight / IPC)
//! - Validates XML byte slices against the parsed schema rules (enumeration, pattern,
//!   minLength/maxLength, minInclusive/maxInclusive for simple types)
//! - Produces structured [`ValidationError`] values — never panics on bad input
//!
//! ## Out of scope (v1)
//!
//! - `xs:key` / `xs:keyref` / `xs:unique` identity constraints
//! - Named model groups: `xs:group ref` is not expanded, so a group's
//!   particles are not part of the types that reference it. The elements a
//!   named group declares appear as top-level (global) elements instead.
//! - Attribute values are not validated, and union values are not checked
//!   against their members' facets
//! - `xs:anyAttribute`
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

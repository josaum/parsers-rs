//! Arrow schema derivation and IPC output for XSD schemas.
//!
//! Derives an Arrow [`Schema`] from a named root element in the parsed
//! [`SchemaModel`], then walks the schema tree to produce flat or nested
//! [`RecordBatch`] streams that can be transported via Arrow Flight.
//!
//! ## Flattening strategy
//!
//! The produced Arrow schema is **flat**: each leaf element and attribute becomes
//! one column. Multi-occurrence particles (`maxOccurs > 1` or unbounded) are
//! emitted as `List<item>`. Nested paths use `"parent.child"` dot-separated names.
//!
//! ## Example
//!
//! ```rust,no_run
//! use xsd_fast_core::{SchemaSet, ParseOptions};
//! use xsd_fast_core::arrow::ArrowOptions;
//!
//! let schema = SchemaSet::load("schemas/schemaV4.xsd", ParseOptions::default())?;
//! let arrow_schema = schema.to_arrow_schema("mensagemschema", ArrowOptions::default())?;
//! println!("{arrow_schema:#?}");
//! # Ok::<(), anyhow::Error>(())
//! ```

use std::sync::Arc;

use anyhow::{Context, Result};
use arrow_array::builder::StringBuilder;
use arrow_array::{ArrayRef, RecordBatch};
use arrow_ipc::writer::{IpcWriteOptions, StreamWriter};
use arrow_schema::{DataType, Field, Schema, SchemaRef, TimeUnit};

use crate::parser::SchemaSet;
use crate::schema::{ComplexType, ContentModel, ElementRef, SchemaModel, SimpleTypeKind, XsdType};

/// Options for Arrow schema derivation.
#[derive(Debug, Clone)]
pub struct ArrowOptions {
    /// Maximum nesting depth to flatten. Deeper elements are emitted as
    /// `Utf8` (raw XML). Default: 8.
    pub max_depth: usize,
    /// Whether to map `xs:date` → `Date32` and `xs:dateTime` → `Timestamp(us, UTC)`.
    /// Default: true.
    pub map_date_types: bool,
    /// Whether to include attributes as columns (prefixed with `@`). Default: true.
    pub include_attributes: bool,
    /// Batch size for IPC writes. Default: 8192.
    pub batch_size: usize,
}

impl Default for ArrowOptions {
    fn default() -> Self {
        Self {
            max_depth: 8,
            map_date_types: true,
            include_attributes: true,
            batch_size: 8_192,
        }
    }
}

impl SchemaSet {
    /// Derive an Arrow [`Schema`] for the named root element.
    ///
    /// The element must be a global element in the schema.
    pub fn to_arrow_schema(&self, root_element: &str, opts: ArrowOptions) -> Result<SchemaRef> {
        let decl = self
            .model
            .global_elements
            .get(root_element)
            .with_context(|| format!("element '{root_element}' not found in global elements"))?;

        let mut fields: Vec<Field> = Vec::new();
        derive_fields_from_type(
            &decl.type_def,
            &self.model,
            &opts,
            0,
            String::new(),
            &mut fields,
        );
        Ok(Arc::new(Schema::new(fields)))
    }

    /// Derive an Arrow schema as a serializable description batch.
    ///
    /// Returns a single [`RecordBatch`] with columns:
    /// `path`, `arrow_type`, `nullable`, `xsd_type`, `enumerations`.
    /// Useful for schema introspection over Flight.
    pub fn to_schema_description_batch(
        &self,
        root_element: &str,
        opts: ArrowOptions,
    ) -> Result<RecordBatch> {
        let schema = self.to_arrow_schema(root_element, opts)?;

        let mut paths = StringBuilder::new();
        let mut arrow_types = StringBuilder::new();
        let mut nullables = StringBuilder::new();

        for field in schema.fields() {
            paths.append_value(field.name());
            arrow_types.append_value(format!("{:?}", field.data_type()));
            nullables.append_value(if field.is_nullable() { "true" } else { "false" });
        }

        let output_schema = Arc::new(Schema::new(vec![
            Field::new("path", DataType::Utf8, false),
            Field::new("arrow_type", DataType::Utf8, false),
            Field::new("nullable", DataType::Utf8, false),
        ]));

        RecordBatch::try_new(
            output_schema,
            vec![
                Arc::new(paths.finish()) as ArrayRef,
                Arc::new(arrow_types.finish()) as ArrayRef,
                Arc::new(nullables.finish()) as ArrayRef,
            ],
        )
        .context("build schema description batch")
    }

    /// Write the schema description for `root_element` to an Arrow IPC stream.
    /// Backwards-compatible default IPC writer for non-Flight consumers.
    pub fn write_schema_ipc(
        &self,
        root_element: &str,
        opts: ArrowOptions,
        out: impl std::io::Write,
    ) -> Result<()> {
        self.write_schema_ipc_with_options(root_element, opts, IpcWriteOptions::default(), out)
    }

    /// Write schema IPC with the caller's explicit Flight policy.
    pub fn write_schema_ipc_with_options(
        &self,
        root_element: &str,
        opts: ArrowOptions,
        write_options: IpcWriteOptions,
        out: impl std::io::Write,
    ) -> Result<()> {
        let batch = self.to_schema_description_batch(root_element, opts)?;
        let mut writer =
            StreamWriter::try_new_with_options(out, batch.schema_ref(), write_options)?;
        writer.write(&batch)?;
        writer.finish()?;
        Ok(())
    }

    /// Derive Arrow schemas for **all** global elements. Returns `(element_name, schema)` pairs.
    pub fn all_arrow_schemas(&self, opts: ArrowOptions) -> Vec<(String, SchemaRef)> {
        self.model
            .global_elements
            .keys()
            .filter_map(|name| {
                self.to_arrow_schema(name, opts.clone())
                    .ok()
                    .map(|s| (name.clone(), s))
            })
            .collect()
    }
}

// ── Field derivation ─────────────────────────────────────────────────────────

fn derive_fields_from_type(
    xsd: &XsdType,
    model: &SchemaModel,
    opts: &ArrowOptions,
    depth: usize,
    prefix: String,
    fields: &mut Vec<Field>,
) {
    if depth > opts.max_depth {
        // Emit a single Utf8 "raw" catch-all.
        fields.push(Field::new(
            if prefix.is_empty() { "value" } else { &prefix },
            DataType::Utf8,
            true,
        ));
        return;
    }

    match xsd {
        XsdType::Simple(st) => {
            let dt = simple_kind_to_arrow(&st.kind, opts);
            let name = if prefix.is_empty() {
                "value".to_string()
            } else {
                prefix
            };
            fields.push(Field::new(name, dt, true));
        }
        XsdType::Complex(ct) => {
            derive_fields_from_complex(ct, model, opts, depth, prefix, fields);
        }
        XsdType::Ref(qn) => {
            if let Some(ct) = model.complex_types.get(&qn.local) {
                // Avoid infinite recursion on self-referential types.
                if depth < opts.max_depth {
                    let cloned = ct.clone();
                    derive_fields_from_complex(&cloned, model, opts, depth, prefix, fields);
                }
            } else if let Some(st) = model.simple_types.get(&qn.local) {
                let dt = simple_kind_to_arrow(&st.kind, opts);
                let name = if prefix.is_empty() {
                    "value".to_string()
                } else {
                    prefix
                };
                fields.push(Field::new(name, dt, true));
            } else {
                // Unresolved — fall back to Utf8.
                let name = if prefix.is_empty() {
                    "value".to_string()
                } else {
                    prefix
                };
                fields.push(Field::new(name, DataType::Utf8, true));
            }
        }
    }
}

fn derive_fields_from_complex(
    ct: &ComplexType,
    model: &SchemaModel,
    opts: &ArrowOptions,
    depth: usize,
    prefix: String,
    fields: &mut Vec<Field>,
) {
    // Attributes first.
    if opts.include_attributes {
        for attr in &ct.attributes {
            let dt = simple_kind_to_arrow(&attr.type_def.kind, opts);
            let name = if prefix.is_empty() {
                format!("@{}", attr.name)
            } else {
                format!("{prefix}.@{}", attr.name)
            };
            fields.push(Field::new(name, dt, !attr.required));
        }
    }

    let particles = match &ct.content {
        ContentModel::Sequence(p) | ContentModel::All(p) | ContentModel::Choice(p) => p,
        ContentModel::SimpleContent(st) => {
            let dt = simple_kind_to_arrow(&st.as_ref().kind, opts);
            let name = if prefix.is_empty() {
                "$text".to_string()
            } else {
                format!("{prefix}.$text")
            };
            fields.push(Field::new(name, dt, true));
            return;
        }
        ContentModel::Empty => return,
    };

    for particle in particles {
        let (child_name, child_type, occ, nillable) = match &particle.element {
            ElementRef::Inline(decl) => {
                let decl = decl.as_ref();
                (
                    decl.name.local.clone(),
                    &decl.type_def,
                    decl.occurrence,
                    decl.nillable,
                )
            }
            ElementRef::Ref(qn) => {
                if let Some(decl) = model.global_elements.get(&qn.local) {
                    (
                        decl.name.local.clone(),
                        &decl.type_def,
                        particle.occurrence,
                        decl.nillable,
                    )
                } else {
                    let col = if prefix.is_empty() {
                        qn.local.clone()
                    } else {
                        format!("{prefix}.{}", qn.local)
                    };
                    fields.push(Field::new(col, DataType::Utf8, true));
                    continue;
                }
            }
            ElementRef::Group(group) => {
                // A nested model group (typically an xs:choice). Flatten each
                // branch into its own column at the current prefix; mark them
                // nullable since at most one branch is selected per instance.
                derive_fields_from_group(group, model, opts, depth, &prefix, fields);
                continue;
            }
        };

        let col_path = if prefix.is_empty() {
            child_name
        } else {
            format!("{prefix}.{child_name}")
        };

        if occ.is_multi() {
            // Wrap the inner type in a List.
            let mut inner_fields: Vec<Field> = Vec::new();
            derive_fields_from_type(
                child_type,
                model,
                opts,
                depth + 1,
                String::new(),
                &mut inner_fields,
            );
            let item_field = if inner_fields.len() == 1 {
                inner_fields
                    .remove(0)
                    .with_name("item")
                    .with_nullable(nillable)
            } else {
                Field::new("item", DataType::Struct(inner_fields.into()), nillable)
            };
            fields.push(Field::new(
                col_path,
                DataType::List(Arc::new(item_field)),
                occ.min == 0,
            ));
        } else {
            let nullable = occ.min == 0 || nillable;
            let mut inner_fields: Vec<Field> = Vec::new();
            derive_fields_from_type(
                child_type,
                model,
                opts,
                depth + 1,
                String::new(),
                &mut inner_fields,
            );

            if inner_fields.len() == 1 {
                let f = inner_fields
                    .remove(0)
                    .with_name(&col_path)
                    .with_nullable(nullable);
                fields.push(f);
            } else if !inner_fields.is_empty() {
                fields.push(Field::new(
                    col_path,
                    DataType::Struct(inner_fields.into()),
                    nullable,
                ));
            }
        }
    }
}

/// Flatten a nested model group's branches into columns at `prefix`.
///
/// Branches of an `xs:choice` are mutually exclusive, so every column produced
/// here is forced nullable regardless of the branch's declared `minOccurs`.
fn derive_fields_from_group(
    group: &ContentModel,
    model: &SchemaModel,
    opts: &ArrowOptions,
    depth: usize,
    prefix: &str,
    fields: &mut Vec<Field>,
) {
    // Reuse the complex-type walk by wrapping the group in a transient type.
    let synthetic = ComplexType {
        name: None,
        content: group.clone(),
        attributes: Vec::new(),
        base: None,
    };
    let start = fields.len();
    derive_fields_from_complex(&synthetic, model, opts, depth, prefix.to_string(), fields);
    // A choice selects at most one branch, so none of its columns are mandatory.
    for field in fields.iter_mut().skip(start) {
        if !field.is_nullable() {
            *field = field.as_ref().clone().with_nullable(true);
        }
    }
}

// ── SimpleTypeKind → Arrow DataType ──────────────────────────────────────────

pub(crate) fn simple_kind_to_arrow(kind: &SimpleTypeKind, opts: &ArrowOptions) -> DataType {
    match kind {
        SimpleTypeKind::Boolean => DataType::Boolean,
        SimpleTypeKind::Integer
        | SimpleTypeKind::Long
        | SimpleTypeKind::UnsignedLong
        | SimpleTypeKind::Int
        | SimpleTypeKind::Short
        | SimpleTypeKind::Byte
        | SimpleTypeKind::UnsignedInt
        | SimpleTypeKind::UnsignedShort
        | SimpleTypeKind::UnsignedByte => DataType::Int64,
        SimpleTypeKind::Decimal | SimpleTypeKind::Float | SimpleTypeKind::Double => {
            DataType::Float64
        }
        SimpleTypeKind::Date if opts.map_date_types => DataType::Date32,
        SimpleTypeKind::DateTime if opts.map_date_types => {
            DataType::Timestamp(TimeUnit::Microsecond, Some("UTC".into()))
        }
        _ => DataType::Utf8,
    }
}

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
use crate::schema::{
    ComplexType, ContentModel, ElementDecl, ElementRef, OccurrenceConstraint, Particle,
    SchemaModel, SimpleTypeKind, XsdType,
};

/// Deepest flattening performed, whatever [`ArrowOptions::max_depth`] asks for.
///
/// Flattening recurses once per level, and a type that refers to itself
/// never runs out of levels, so the caller's value alone cannot bound the
/// recursion. Real schemas are flattened a handful of levels deep.
const MAX_FLATTEN_DEPTH: usize = 32;

/// Deepest recursion of one derivation, counting nested model groups.
///
/// Model groups nest without adding a flattening level, so they are counted
/// separately: this bounds the stack of a derivation, where
/// [`MAX_FLATTEN_DEPTH`] bounds its output. Sized, like the parser's nesting
/// limit, to fit a 256 KiB stack in an unoptimized build.
const MAX_DERIVATION_NESTING: usize = 64;

/// Most steps one derivation may take: types visited plus fields emitted.
///
/// A type with `n` children of its own type flattens to `n^max_depth`
/// columns, so a schema of a few hundred bytes can ask for billions, and the
/// walk costs that much even where it emits nothing. A derived schema holds
/// at most one field per step; no consumer can materialize a table this
/// wide, so a derivation that needs more steps is refused instead of run.
const MAX_DERIVATION_STEPS: usize = 100_000;

/// Options for Arrow schema derivation.
#[derive(Debug, Clone)]
pub struct ArrowOptions {
    /// Maximum nesting depth to flatten. Deeper elements are emitted as
    /// `Utf8` (raw XML). Default: 8. Values above 32 are treated as 32.
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
    ///
    /// # Errors
    ///
    /// Fails when `root_element` is not a global element, and with a
    /// `resource_limit` error when flattening would take more than 100,000
    /// steps (types visited plus fields emitted) or its model groups nest too
    /// deeply to walk.
    pub fn to_arrow_schema(&self, root_element: &str, opts: ArrowOptions) -> Result<SchemaRef> {
        let decl = self
            .model
            .global_elements
            .get(root_element)
            .with_context(|| format!("element '{root_element}' not found in global elements"))?;

        let mut fields: Vec<Field> = Vec::new();
        let mut derivation = Derivation {
            model: &self.model,
            max_depth: opts.max_depth.min(MAX_FLATTEN_DEPTH),
            opts: &opts,
            steps_left: MAX_DERIVATION_STEPS,
            nesting: 0,
        };
        derivation.fields_from_type(&decl.type_def, 0, String::new(), &mut fields)?;
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

/// State of one schema derivation: its inputs and what is left of its budgets.
struct Derivation<'a> {
    model: &'a SchemaModel,
    opts: &'a ArrowOptions,
    /// [`ArrowOptions::max_depth`], capped at [`MAX_FLATTEN_DEPTH`].
    max_depth: usize,
    /// Steps that may still be taken before the derivation is refused.
    steps_left: usize,
    /// Content models currently being walked.
    nesting: usize,
}

impl Derivation<'_> {
    /// Charges one step to the derivation budget.
    fn step(&mut self) -> Result<()> {
        self.steps_left = self.steps_left.checked_sub(1).with_context(|| {
            format!(
                "resource_limit: arrow_derivation_steps: flattening takes more than {MAX_DERIVATION_STEPS} steps"
            )
        })?;
        Ok(())
    }

    /// Appends `field`, charging it to the derivation budget.
    fn emit(&mut self, fields: &mut Vec<Field>, field: Field) -> Result<()> {
        self.step()?;
        fields.push(field);
        Ok(())
    }

    // The functions below recurse once per flattening level and once per
    // nested model group. Everything that builds a `Field` lives in a helper
    // that returns before the recursion continues, so a level costs a few
    // hundred bytes of stack instead of every temporary of every branch.

    fn fields_from_type(
        &mut self,
        xsd: &XsdType,
        depth: usize,
        prefix: String,
        fields: &mut Vec<Field>,
    ) -> Result<()> {
        self.step()?;
        if depth > self.max_depth {
            // Emit a single Utf8 "raw" catch-all.
            return self.emit_leaf(prefix, None, fields);
        }

        let model = self.model;
        match xsd {
            XsdType::Simple(st) => self.emit_leaf(prefix, Some(&st.kind), fields),
            XsdType::Complex(ct) => self.fields_from_complex(ct, depth, &prefix, fields),
            XsdType::Ref(qn) => {
                if let Some(ct) = model.complex_types.get(&qn.local) {
                    // Avoid infinite recursion on self-referential types.
                    if depth < self.max_depth {
                        self.fields_from_complex(ct, depth, &prefix, fields)?;
                    }
                    Ok(())
                } else {
                    // A named simple type, or unresolved: fall back to Utf8.
                    let kind = model.simple_types.get(&qn.local).map(|st| &st.kind);
                    self.emit_leaf(prefix, kind, fields)
                }
            }
        }
    }

    /// Emits the single column of a leaf; `kind` is `None` for raw text.
    fn emit_leaf(
        &mut self,
        prefix: String,
        kind: Option<&SimpleTypeKind>,
        fields: &mut Vec<Field>,
    ) -> Result<()> {
        let data_type = match kind {
            Some(kind) => simple_kind_to_arrow(kind, self.opts),
            None => DataType::Utf8,
        };
        self.emit(fields, Field::new(value_name(prefix), data_type, true))
    }

    fn fields_from_complex(
        &mut self,
        ct: &ComplexType,
        depth: usize,
        prefix: &str,
        fields: &mut Vec<Field>,
    ) -> Result<()> {
        // Attributes first.
        if self.opts.include_attributes {
            self.emit_attributes(ct, prefix, fields)?;
        }
        self.fields_from_content(&ct.content, depth, prefix, fields)
    }

    fn emit_attributes(
        &mut self,
        ct: &ComplexType,
        prefix: &str,
        fields: &mut Vec<Field>,
    ) -> Result<()> {
        for attr in &ct.attributes {
            let dt = simple_kind_to_arrow(&attr.type_def.kind, self.opts);
            let name = if prefix.is_empty() {
                format!("@{}", attr.name)
            } else {
                format!("{prefix}.@{}", attr.name)
            };
            self.emit(fields, Field::new(name, dt, !attr.required))?;
        }
        Ok(())
    }

    /// Flattens one content model into columns at `prefix`.
    ///
    /// This is the only function a nested model group re-enters, so it is
    /// where the recursion of a derivation is counted and refused.
    fn fields_from_content(
        &mut self,
        content: &ContentModel,
        depth: usize,
        prefix: &str,
        fields: &mut Vec<Field>,
    ) -> Result<()> {
        if self.nesting >= MAX_DERIVATION_NESTING {
            anyhow::bail!(
                "resource_limit: arrow_nesting_depth: content models nest deeper than {MAX_DERIVATION_NESTING}"
            );
        }
        let particles = match content {
            ContentModel::Sequence(p) | ContentModel::All(p) | ContentModel::Choice(p) => p,
            ContentModel::SimpleContent(st) => return self.emit_text(&st.kind, prefix, fields),
            ContentModel::Empty => return Ok(()),
        };
        self.nesting += 1;
        let result = self.fields_from_particles(particles, depth, prefix, fields);
        self.nesting -= 1;
        result
    }

    /// Emits the `$text` column of a simple-content type.
    fn emit_text(
        &mut self,
        kind: &SimpleTypeKind,
        prefix: &str,
        fields: &mut Vec<Field>,
    ) -> Result<()> {
        let dt = simple_kind_to_arrow(kind, self.opts);
        let name = if prefix.is_empty() {
            "$text".to_string()
        } else {
            format!("{prefix}.$text")
        };
        self.emit(fields, Field::new(name, dt, true))
    }

    fn fields_from_particles(
        &mut self,
        particles: &[Particle],
        depth: usize,
        prefix: &str,
        fields: &mut Vec<Field>,
    ) -> Result<()> {
        let model = self.model;
        for particle in particles {
            match &particle.element {
                ElementRef::Inline(decl) => {
                    self.fields_from_child(decl, decl.occurrence, depth, prefix, fields)?;
                }
                ElementRef::Ref(qn) => match model.global_elements.get(&qn.local) {
                    Some(decl) => {
                        self.fields_from_child(decl, particle.occurrence, depth, prefix, fields)?;
                    }
                    None => self.emit_unresolved_element(&qn.local, prefix, fields)?,
                },
                // A nested model group (typically an xs:choice). Flatten each
                // branch into its own column at the current prefix; mark them
                // nullable since at most one branch is selected per instance.
                ElementRef::Group(group) => self.fields_from_group(group, depth, prefix, fields)?,
            }
        }
        Ok(())
    }

    /// Emits a `Utf8` column for a reference to an element that is not declared.
    fn emit_unresolved_element(
        &mut self,
        local: &str,
        prefix: &str,
        fields: &mut Vec<Field>,
    ) -> Result<()> {
        self.emit(
            fields,
            Field::new(column_path(prefix, local), DataType::Utf8, true),
        )
    }

    /// Flattens one child element, then wraps what it produced into a column.
    fn fields_from_child(
        &mut self,
        decl: &ElementDecl,
        occ: OccurrenceConstraint,
        depth: usize,
        prefix: &str,
        fields: &mut Vec<Field>,
    ) -> Result<()> {
        let mut inner_fields: Vec<Field> = Vec::new();
        self.fields_from_type(&decl.type_def, depth + 1, String::new(), &mut inner_fields)?;
        self.emit_child(decl, occ, prefix, inner_fields, fields)
    }

    /// Appends the column for one child element from its flattened type.
    fn emit_child(
        &mut self,
        decl: &ElementDecl,
        occ: OccurrenceConstraint,
        prefix: &str,
        mut inner_fields: Vec<Field>,
        fields: &mut Vec<Field>,
    ) -> Result<()> {
        let col_path = column_path(prefix, &decl.name.local);
        let nillable = decl.nillable;
        if occ.is_multi() {
            // Wrap the inner type in a List.
            let item_field = if inner_fields.len() == 1 {
                inner_fields
                    .remove(0)
                    .with_name("item")
                    .with_nullable(nillable)
            } else {
                Field::new("item", DataType::Struct(inner_fields.into()), nillable)
            };
            self.emit(
                fields,
                Field::new(col_path, DataType::List(Arc::new(item_field)), occ.min == 0),
            )
        } else {
            let nullable = occ.min == 0 || nillable;
            if inner_fields.len() == 1 {
                let f = inner_fields
                    .remove(0)
                    .with_name(&col_path)
                    .with_nullable(nullable);
                // Already charged when the inner walk emitted it.
                fields.push(f);
                Ok(())
            } else if !inner_fields.is_empty() {
                self.emit(
                    fields,
                    Field::new(col_path, DataType::Struct(inner_fields.into()), nullable),
                )
            } else {
                Ok(())
            }
        }
    }

    /// Flatten a nested model group's branches into columns at `prefix`.
    ///
    /// Branches of an `xs:choice` are mutually exclusive, so every column produced
    /// here is forced nullable regardless of the branch's declared `minOccurs`.
    fn fields_from_group(
        &mut self,
        group: &ContentModel,
        depth: usize,
        prefix: &str,
        fields: &mut Vec<Field>,
    ) -> Result<()> {
        let start = fields.len();
        self.fields_from_content(group, depth, prefix, fields)?;
        force_nullable(&mut fields[start..]);
        Ok(())
    }
}

/// Marks every field nullable: a choice selects at most one branch, so none
/// of its columns are mandatory.
fn force_nullable(fields: &mut [Field]) {
    for field in fields {
        if !field.is_nullable() {
            *field = field.clone().with_nullable(true);
        }
    }
}

/// `prefix.name`, or `name` at the top level.
fn column_path(prefix: &str, name: &str) -> String {
    if prefix.is_empty() {
        name.to_string()
    } else {
        format!("{prefix}.{name}")
    }
}

/// Column name of a leaf with no enclosing element: the prefix, or `value`.
fn value_name(prefix: String) -> String {
    if prefix.is_empty() {
        "value".to_string()
    } else {
        prefix
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

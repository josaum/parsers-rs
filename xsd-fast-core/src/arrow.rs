//! Arrow schema derivation and IPC output for XSD schemas.
//!
//! Derives an Arrow [`Schema`] from a named root element in the parsed
//! [`SchemaModel`], then walks the schema tree to produce flat or nested
//! [`RecordBatch`] streams that can be transported via Arrow Flight.
//!
//! ## Flattening strategy
//!
//! Each leaf element and each attribute of the root element becomes one
//! column, and a child element with more than one column becomes a struct of
//! them. A child element with exactly one column is that column, named after
//! the element. Multi-occurrence particles (`maxOccurs > 1` or unbounded) are
//! emitted as `List<item>`. Wildcards (`xs:any`) derive no column.
//!
//! ## One column per name
//!
//! Sibling columns never share a name, so every column can be read by name
//! (pyarrow's `StructArray.field(name)` raised `KeyError` and polars
//! `DuplicateError` on the shape this replaces). Elements of one model group
//! that share a local name are one column, at the position of the first:
//!
//! - In an `xs:choice` that occurs at most once, each branch holds the name
//!   at most once and only one branch occurs, so the column is a scalar of
//!   their common type, unless one of them repeats or lies in a model group
//!   that repeats.
//! - Anywhere else (a sequence, `xs:all`, a repeated choice, or a choice
//!   branch that repeats) the column is a `List` of their common type, so
//!   `<a/><b/><a/>` reads as one list `a`. A repeated particle contributes
//!   its items, not a nested list; any other column that is a list (a child
//!   whose one column is a list, or an `xs:list` value) is one item, so the
//!   merged column is a list of lists. The list is nullable only when every
//!   element it merges is optional.
//!
//! A nested group is merged first, then its columns with those of the group
//! around it, so inherited particles merge with the ones a type adds. Types
//! are common when they are equal up to nullability; the merged type is
//! nullable wherever one of them is. A child element two of whose own
//! columns merged stays a struct, even with one column left, so no column
//! path is lost. A column of referenced attributes only (see below) that
//! merges with one of the child's own columns does not count as a merge: the
//! child keeps the shape it had, though its one column may become a list.
//!
//! Elements whose types differ keep their own columns and positions, and
//! every one after the first is renamed `name#2`, `name#3`, ... in document
//! order, taking the least number that no column of the group holds (a
//! nested group's rename included), and numbering from the name before any
//! `#`, so a name never carries two. `#` cannot occur in an XML name, as `@`
//! (attributes) and `$` (`$text`) cannot. Particles are matched by local
//! name: the model does not keep the namespace of an element particle.
//!
//! ## Nullability under a choice
//!
//! A column a choice derives is nullable unless the choice is required and
//! the column's name occurs in every branch. Every column of an optional
//! group is nullable, and so is every column of a nested sequence that does
//! not occur exactly once.
//!
//! An attribute that reaches a type through `xs:attribute ref` or
//! `xs:attributeGroup ref` is a column of the root element, and of a child
//! element with no column or more than one column of its own. **By default
//! it is left out of a child element whose other content derives exactly one
//! column** (typically its text), so that column keeps the shape it had
//! before references were resolved; [`ArrowShape::attributes_on_simple_children`]
//! makes such a child a struct of its value and all its attributes.
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
    SchemaModel, SimpleTypeDef, SimpleTypeKind, XsdType,
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

/// Most walk steps one derivation may take before it is refused.
///
/// A step is one type visited or one field emitted. Fields that are later
/// wrapped into a struct or a list are counted, and so are visits that emit
/// nothing, so this bounds the work of a derivation and not the width of its
/// result: the S1000D Issue 4.2 schemas cost about 3.4 steps per node of the
/// derived schema, and a flat type costs two steps per column.
///
/// The bound exists because a type with `n` children of its own type
/// flattens to `n^max_depth` columns, so a schema of a few hundred bytes can
/// ask for billions of steps, and the walk costs that much even where it
/// emits nothing. The largest element of S1000D Issue 4.2 needs about
/// 112,000 steps; the limit is roughly nine times that. A derivation that
/// reaches it has run for about a third of a second and holds under 100 MiB
/// in an unoptimized build. Lowering the limit turns large published schemas
/// into errors, so a new value needs the Arrow resource-limit tests rerun.
const MAX_DERIVATION_STEPS: usize = 1_000_000;

/// Most walk steps one call that derives every global element may take.
///
/// Each element is still held to [`MAX_DERIVATION_STEPS`], but a schema can
/// declare thousands of elements that each stay under it, and the call keeps
/// every result: 2,000 elements of 1,000 columns are 2,000,000 fields from a
/// schema of about 118 KiB. Four elements' worth bounds what one call can
/// build to a few hundred MiB in an unoptimized build. The real schema sets
/// checked when this was set (ANS TISS, S1000D Issue 6) derive every element
/// inside it. Lowering it turns large published schemas into errors.
const MAX_TOTAL_DERIVATION_STEPS: usize = 4 * MAX_DERIVATION_STEPS;

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

/// How Arrow derivation shapes columns, beyond [`ArrowOptions`].
///
/// Kept apart from `ArrowOptions` so code that builds `ArrowOptions` field
/// by field keeps compiling. Pass it to
/// [`SchemaSet::to_arrow_schema_with_shape`] or
/// [`SchemaSet::try_all_arrow_schemas_with_shape`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct ArrowShape {
    /// Whether a child element whose own content derives exactly one column
    /// also gets the attributes that reach its type through
    /// `xs:attribute ref` or `xs:attributeGroup ref`. Default: false.
    ///
    /// By default such a child is that one column, named after the element,
    /// as it was before references were resolved, and those attributes are
    /// left out: they are columns of the same type only where it is the
    /// root element, or a child with no column or several of its own. When
    /// true, the child becomes a struct of its value and all its
    /// attributes, which changes the type of its column.
    pub attributes_on_simple_children: bool,
}

impl ArrowShape {
    /// The default shape, which [`SchemaSet::to_arrow_schema`] uses.
    ///
    /// A child element whose own content derives one column is that column,
    /// without the attributes that reach its type by reference. Sibling
    /// elements of one name are one column whatever the shape; see the
    /// module documentation, "One column per name".
    pub fn new() -> Self {
        Self::default()
    }

    /// Sets [`ArrowShape::attributes_on_simple_children`].
    #[must_use]
    pub fn with_attributes_on_simple_children(mut self, on: bool) -> Self {
        self.attributes_on_simple_children = on;
        self
    }
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
    /// `resource_limit` error when flattening would take more than 1,000,000
    /// walk steps (types visited plus fields emitted, wrapped ones included)
    /// or its model groups nest too deeply to walk.
    pub fn to_arrow_schema(&self, root_element: &str, opts: ArrowOptions) -> Result<SchemaRef> {
        self.to_arrow_schema_with_shape(root_element, opts, ArrowShape::default())
    }

    /// Derive an Arrow [`Schema`] for the named root element, shaped as
    /// `shape` says; [`SchemaSet::to_arrow_schema`] uses the default shape.
    ///
    /// # Errors
    ///
    /// As [`SchemaSet::to_arrow_schema`].
    pub fn to_arrow_schema_with_shape(
        &self,
        root_element: &str,
        opts: ArrowOptions,
        shape: ArrowShape,
    ) -> Result<SchemaRef> {
        let mut steps = MAX_DERIVATION_STEPS;
        self.derive_arrow_schema(root_element, &opts, shape, &mut steps)
    }

    /// Derives one element's schema, taking its steps out of `steps_left`.
    ///
    /// The derivation may use what `steps_left` holds, up to
    /// [`MAX_DERIVATION_STEPS`]; what it used is deducted whether it succeeds
    /// or fails.
    fn derive_arrow_schema(
        &self,
        root_element: &str,
        opts: &ArrowOptions,
        shape: ArrowShape,
        steps_left: &mut usize,
    ) -> Result<SchemaRef> {
        let decl = self
            .model
            .global_elements
            .get(root_element)
            .with_context(|| format!("element '{root_element}' not found in global elements"))?;

        let allowed = (*steps_left).min(MAX_DERIVATION_STEPS);
        let mut fields: Vec<Field> = Vec::new();
        let mut derivation = Derivation {
            model: &self.model,
            max_depth: opts.max_depth.min(MAX_FLATTEN_DEPTH),
            opts,
            shape,
            steps_left: allowed,
            nesting: 0,
        };
        let walked = derivation.fields_from_type(&decl.type_def, 0, String::new(), &mut fields);
        *steps_left -= allowed - derivation.steps_left;
        walked?;
        let fields = fields.into_iter().map(unmark).collect::<Vec<_>>();
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

    /// Derive Arrow schemas for the global elements that can be derived.
    ///
    /// Returns `(element_name, schema)` pairs in declaration order. An element
    /// whose derivation fails, which happens when it exceeds a resource limit
    /// (see [`SchemaSet::to_arrow_schema`]), is left out without a report, so
    /// the result can hold fewer entries than the schema has global elements.
    /// The call as a whole may take 4,000,000 walk steps; once they are used,
    /// the elements not yet derived are left out as well.
    /// Use [`SchemaSet::try_all_arrow_schemas`] to learn of either failure.
    pub fn all_arrow_schemas(&self, opts: ArrowOptions) -> Vec<(String, SchemaRef)> {
        let mut steps_left = MAX_TOTAL_DERIVATION_STEPS;
        let mut derived = Vec::new();
        for name in self.model.global_elements.keys() {
            if steps_left == 0 {
                break;
            }
            if let Ok(schema) =
                self.derive_arrow_schema(name, &opts, ArrowShape::default(), &mut steps_left)
            {
                derived.push((name.clone(), schema));
            }
        }
        derived
    }

    /// Derive Arrow schemas for **all** global elements, or fail.
    ///
    /// Returns `(element_name, schema)` pairs in declaration order, one for
    /// every global element.
    ///
    /// # Errors
    ///
    /// Fails on the first element whose derivation fails. The message is the
    /// one [`SchemaSet::to_arrow_schema`] gives, so it still starts with
    /// `resource_limit` for a resource limit, followed by the name of the
    /// element. Fails with `resource_limit: arrow_derivation_steps_total` when
    /// the elements together take more than 4,000,000 walk steps, even if
    /// each is inside its own limit. No partial result is returned.
    pub fn try_all_arrow_schemas(&self, opts: ArrowOptions) -> Result<Vec<(String, SchemaRef)>> {
        self.try_all_arrow_schemas_with_shape(opts, ArrowShape::default())
    }

    /// [`SchemaSet::try_all_arrow_schemas`], shaped as `shape` says.
    ///
    /// # Errors
    ///
    /// As [`SchemaSet::try_all_arrow_schemas`].
    pub fn try_all_arrow_schemas_with_shape(
        &self,
        opts: ArrowOptions,
        shape: ArrowShape,
    ) -> Result<Vec<(String, SchemaRef)>> {
        let mut steps_left = MAX_TOTAL_DERIVATION_STEPS;
        let mut derived = Vec::with_capacity(self.model.global_elements.len());
        for name in self.model.global_elements.keys() {
            let before = steps_left;
            match self.derive_arrow_schema(name, &opts, shape, &mut steps_left) {
                Ok(schema) => derived.push((name.clone(), schema)),
                // The element ran out because the call did, not because it
                // is too large on its own.
                Err(_) if steps_left == 0 && before < MAX_DERIVATION_STEPS => {
                    return Err(anyhow::anyhow!(
                        "resource_limit: arrow_derivation_steps_total: deriving every global \
                         element takes more than {MAX_TOTAL_DERIVATION_STEPS} steps (reached at \
                         element '{name}')"
                    ));
                }
                Err(error) => return Err(anyhow::anyhow!("{error:#} (element '{name}')")),
            }
        }
        Ok(derived)
    }
}

// ── Field derivation ─────────────────────────────────────────────────────────

/// State of one schema derivation: its inputs and what is left of its budgets.
struct Derivation<'a> {
    model: &'a SchemaModel,
    opts: &'a ArrowOptions,
    shape: ArrowShape,
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
            XsdType::Simple(st) => self.emit_leaf(prefix, Some(st), fields),
            XsdType::Complex(ct) => self.fields_from_complex(ct, depth, &prefix, fields),
            XsdType::Ref(qn) => {
                if let Some(ct) = model.complex_types.get(&qn.local) {
                    // Avoid infinite recursion on self-referential types. A
                    // type that declares nothing has nowhere to recurse, and
                    // keeps its marker column at the flattening limit too.
                    if depth < self.max_depth || declares_nothing(ct) {
                        self.fields_from_complex(ct, depth, &prefix, fields)?;
                    }
                    Ok(())
                } else {
                    // A named simple type, or unresolved: fall back to Utf8.
                    let simple = model.simple_types.get(&qn.local);
                    self.emit_leaf(prefix, simple, fields)
                }
            }
        }
    }

    /// Emits the single column of a leaf; `simple` is `None` for raw text.
    fn emit_leaf(
        &mut self,
        prefix: String,
        simple: Option<&SimpleTypeDef>,
        fields: &mut Vec<Field>,
    ) -> Result<()> {
        let data_type = match simple {
            Some(simple) => simple_type_to_arrow(simple, self.opts),
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
        // A type that declares nothing makes its element a marker: all the
        // element says is that it is there. It keeps a text column, as an
        // element of an unknown type does, so the marker is not dropped and
        // a parent holding it keeps its shape.
        if declares_nothing(ct) {
            return self.emit_leaf(prefix.to_string(), None, fields);
        }
        // Attributes first.
        if self.opts.include_attributes {
            self.emit_attributes(ct, prefix, fields)?;
        }
        self.fields_from_content(&ct.content, ct.content_occurrence, depth, prefix, fields)
    }

    fn emit_attributes(
        &mut self,
        ct: &ComplexType,
        prefix: &str,
        fields: &mut Vec<Field>,
    ) -> Result<()> {
        for attr in &ct.attributes {
            let dt = simple_type_to_arrow(&attr.type_def, self.opts);
            let name = if prefix.is_empty() {
                format!("@{}", attr.name)
            } else {
                format!("{prefix}.@{}", attr.name)
            };
            let mut field = Field::new(name, dt, !attr.required);
            if attr.referenced {
                field = mark_referenced(field);
            }
            self.emit(fields, field)?;
        }
        Ok(())
    }

    /// Flattens one content model, which occurs `occ` times, into columns
    /// at `prefix`.
    ///
    /// This is the only function a nested model group re-enters, so it is
    /// where the recursion of a derivation is counted and refused.
    fn fields_from_content(
        &mut self,
        content: &ContentModel,
        occ: OccurrenceConstraint,
        depth: usize,
        prefix: &str,
        fields: &mut Vec<Field>,
    ) -> Result<()> {
        if self.nesting >= MAX_DERIVATION_NESTING {
            anyhow::bail!(
                "resource_limit: arrow_nesting_depth: content models nest deeper than {MAX_DERIVATION_NESTING}"
            );
        }
        let (particles, choice) = match content {
            ContentModel::Sequence(p) | ContentModel::All(p) => (p, false),
            ContentModel::Choice(p) => (p, true),
            ContentModel::SimpleContent(st) => return self.emit_text(st, prefix, fields),
            ContentModel::Empty => return Ok(()),
        };
        let start = fields.len();
        // Where each branch of a choice ends in `fields`.
        let mut ends = Vec::new();
        self.nesting += 1;
        let result = self.fields_from_particles(
            particles,
            depth,
            prefix,
            fields,
            choice.then_some(&mut ends),
        );
        self.nesting -= 1;
        result?;
        if choice {
            settle_choice(fields, start, &ends, occ);
        } else {
            merge_group_columns(fields, start, false);
        }
        Ok(())
    }

    /// Emits the `$text` column of a simple-content type.
    fn emit_text(
        &mut self,
        simple: &SimpleTypeDef,
        prefix: &str,
        fields: &mut Vec<Field>,
    ) -> Result<()> {
        let dt = simple_type_to_arrow(simple, self.opts);
        let name = if prefix.is_empty() {
            "$text".to_string()
        } else {
            format!("{prefix}.$text")
        };
        self.emit(fields, Field::new(name, dt, true))
    }

    /// Flattens `particles` into columns at `prefix`, pushing to `ends`,
    /// when given, the length of `fields` after each particle.
    fn fields_from_particles(
        &mut self,
        particles: &[Particle],
        depth: usize,
        prefix: &str,
        fields: &mut Vec<Field>,
        mut ends: Option<&mut Vec<usize>>,
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
                // A nested model group: its columns join the current prefix.
                ElementRef::Group(group) => {
                    self.fields_from_group(group, particle.occurrence, depth, prefix, fields)?;
                }
                // A wildcard admits elements the schema does not declare.
                ElementRef::Any(_) => {}
            }
            if let Some(ends) = ends.as_deref_mut() {
                ends.push(fields.len());
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
        let own = inner_fields.iter().filter(|f| !is_referenced(f)).count();
        // Columns of its own merged into one were several before the merge,
        // and keep the struct and the attributes they had. Merged columns
        // made only of referenced attributes are not its own, as before.
        let merged = inner_fields
            .iter()
            .any(|f| is_merged(f) && !is_referenced(f));
        let single = own == 1 && !merged;
        if single && !self.shape.attributes_on_simple_children {
            // A child whose other content derives one column keeps the shape
            // it had before attribute references were resolved: that column,
            // named after the child.
            inner_fields.retain(|f| !is_referenced(f));
        }
        let mut inner_fields: Vec<Field> = inner_fields.into_iter().map(unmark).collect();
        // A repeated child with no column of its own was a list of empty
        // structs; one referenced attribute keeps it a list of structs.
        let keep_struct = (own == 0 && occ.is_multi()) || merged;
        if occ.is_multi() {
            // Wrap the inner type in a List.
            let item_field = if inner_fields.len() == 1 && !keep_struct {
                inner_fields
                    .remove(0)
                    .with_name("item")
                    .with_nullable(nillable)
            } else {
                Field::new("item", DataType::Struct(inner_fields.into()), nillable)
            };
            let list = Field::new(col_path, DataType::List(Arc::new(item_field)), occ.min == 0);
            self.emit(fields, with_mark(list, REPEATED_MARK))
        } else {
            let nullable = occ.min == 0 || nillable;
            // A child with no column of its own had no column at all before
            // attribute references were resolved: its column is as new as
            // the attributes that make it, to the parent too.
            let new = |field: Field| {
                if own == 0 {
                    mark_referenced(field)
                } else {
                    field
                }
            };
            if inner_fields.len() == 1 && !merged {
                let f = inner_fields
                    .remove(0)
                    .with_name(&col_path)
                    .with_nullable(nullable);
                // Already charged when the inner walk emitted it.
                fields.push(new(f));
                Ok(())
            } else if !inner_fields.is_empty() {
                let f = Field::new(col_path, DataType::Struct(inner_fields.into()), nullable);
                self.emit(fields, new(f))
            } else {
                Ok(())
            }
        }
    }

    /// Flattens a nested model group, which occurs `occ` times, into
    /// columns at `prefix`.
    ///
    /// A choice sets the nullability of its own columns (see
    /// [`settle_choice`]). Every column of an optional group is nullable, and
    /// so is every column of a sequence that does not occur exactly once,
    /// which may be left out whole or hold children another repetition lacks.
    fn fields_from_group(
        &mut self,
        group: &ContentModel,
        occ: OccurrenceConstraint,
        depth: usize,
        prefix: &str,
        fields: &mut Vec<Field>,
    ) -> Result<()> {
        let start = fields.len();
        self.fields_from_content(group, occ, depth, prefix, fields)?;
        let is_choice = matches!(group, ContentModel::Choice(_));
        let derived = fields.get_mut(start..).unwrap_or_default();
        if occ.min == 0 || (!is_choice && occ != OccurrenceConstraint::default()) {
            force_nullable(derived);
        }
        if occ.is_multi() {
            // A choice around the group cannot merge these into a scalar.
            for field in derived {
                *field = with_mark(field.clone(), IN_REPEAT_MARK);
            }
        }
        Ok(())
    }
}

/// Whether a complex type is empty as written, and has stayed so.
///
/// True for `<xs:complexType/>` and for the open-and-closed form with
/// nothing inside but annotations. Having no particles and no attributes is
/// not enough: the parser leaves out the constructs it does not model, so a
/// type built only from those has neither, and derives no column.
fn declares_nothing(ct: &ComplexType) -> bool {
    ct.empty_in_source
        && ct.base.is_none()
        && ct.attributes.is_empty()
        && matches!(ct.content, ContentModel::Empty)
}

/// Metadata key marking, during one derivation, a column of a referenced
/// attribute; see [`crate::schema::AttributeDecl::referenced`]. It never
/// reaches the derived schema.
const REFERENCED_MARK: &str = "xsd-fast-core.referenced";

/// Metadata key marking, during one derivation, the `List` column of a
/// repeated particle, as against a list-typed value (`xs:list`). It never
/// reaches the derived schema.
const REPEATED_MARK: &str = "xsd-fast-core.repeated";

/// Metadata key marking, during one derivation, a column merged from sibling
/// columns of one name. It never reaches the derived schema.
const MERGED_MARK: &str = "xsd-fast-core.merged";

/// Metadata key marking, during one derivation, a column derived inside a
/// model group that repeats, whose element may occur more than once
/// although its column is not a list. It never reaches the derived schema.
const IN_REPEAT_MARK: &str = "xsd-fast-core.in-repeat";

/// Every mark a derivation may set; see [`unmark`].
const MARKS: [&str; 4] = [REFERENCED_MARK, REPEATED_MARK, MERGED_MARK, IN_REPEAT_MARK];

/// `field` with the derivation mark `mark` added to its metadata.
fn with_mark(field: Field, mark: &str) -> Field {
    let mut metadata = field.metadata().clone();
    metadata.insert(mark.to_owned(), String::new());
    field.with_metadata(metadata)
}

/// `field` marked as the column of a referenced attribute.
fn mark_referenced(field: Field) -> Field {
    with_mark(field, REFERENCED_MARK)
}

/// Whether `field` is the column of a referenced attribute.
fn is_referenced(field: &Field) -> bool {
    field.metadata().contains_key(REFERENCED_MARK)
}

/// Whether `field` is the `List` column of a repeated particle.
fn is_repeated(field: &Field) -> bool {
    field.metadata().contains_key(REPEATED_MARK)
}

/// Whether `field` was derived inside a model group that repeats.
fn in_repeat(field: &Field) -> bool {
    field.metadata().contains_key(IN_REPEAT_MARK)
}

/// Whether `field` was merged from sibling columns of one name.
fn is_merged(field: &Field) -> bool {
    field.metadata().contains_key(MERGED_MARK)
}

/// `field` without any derivation mark.
fn unmark(field: Field) -> Field {
    if MARKS
        .iter()
        .any(|mark| field.metadata().contains_key(*mark))
    {
        let mut metadata = field.metadata().clone();
        for mark in MARKS {
            metadata.remove(mark);
        }
        field.with_metadata(metadata)
    } else {
        field
    }
}

/// Merges and settles the columns a choice emitted at `fields[start..]`.
///
/// `ends` holds where each branch's columns end. A choice that occurs at
/// most once is exclusive: its columns of one name, at most one per branch,
/// merge into one scalar unless one of them repeats. A column is nullable
/// unless it occurs in every branch of a choice that is required.
fn settle_choice(fields: &mut Vec<Field>, start: usize, ends: &[usize], occ: OccurrenceConstraint) {
    // The names each branch derives.
    let mut branches: Vec<std::collections::HashSet<String>> = Vec::with_capacity(ends.len());
    let mut from = start;
    for &end in ends {
        let names = fields.get(from..end).unwrap_or_default();
        branches.push(names.iter().map(|f| f.name().clone()).collect());
        from = end;
    }
    merge_group_columns(fields, start, !occ.is_multi());
    for field in fields.iter_mut().skip(start) {
        let in_every = branches.iter().all(|names| names.contains(field.name()));
        if (occ.min == 0 || !in_every) && !field.is_nullable() {
            *field = field.clone().with_nullable(true);
        }
    }
}

/// Merges the columns one model group emitted at `fields[start..]` that
/// share a name; see the module documentation, "One column per name".
///
/// `exclusive` is true for a choice that occurs at most once. A merged
/// column stands where its first part stood; parts that cannot merge keep
/// their own positions. Names are taken in document order, so the result
/// does not depend on hashing.
fn merge_group_columns(fields: &mut Vec<Field>, start: usize, exclusive: bool) {
    let group = fields.get(start..).unwrap_or_default();
    let mut seen = std::collections::HashSet::with_capacity(group.len());
    if group.iter().all(|f| seen.insert(f.name().as_str())) {
        return;
    }
    let mut slots: Vec<Option<Field>> = fields.split_off(start).into_iter().map(Some).collect();
    // Positions of the columns of each name, names in order of first appearance.
    let mut positions: Vec<Vec<usize>> = Vec::new();
    let mut slot: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    for (at, field) in slots.iter().enumerate() {
        let Some(field) = field else { continue };
        match slot.get(field.name()) {
            Some(&group) => {
                if let Some(at_all) = positions.get_mut(group) {
                    at_all.push(at);
                }
            }
            None => {
                slot.insert(field.name().clone(), positions.len());
                positions.push(vec![at]);
            }
        }
    }
    // Names a rename must not take: every name of the group, and each
    // rename made so far.
    let mut used: std::collections::HashSet<String> = slot.into_keys().collect();
    for at_all in positions.iter().filter(|at_all| at_all.len() > 1) {
        let parts: Vec<Field> = at_all
            .iter()
            .filter_map(|&at| slots.get_mut(at).and_then(Option::take))
            .collect();
        match merge_columns(&parts, exclusive) {
            Some(merged) => {
                if let Some(first) = at_all.first().and_then(|&at| slots.get_mut(at)) {
                    *first = Some(merged);
                }
            }
            None => {
                for (index, (&at, part)) in at_all.iter().zip(parts).enumerate() {
                    let part = if index == 0 {
                        part
                    } else {
                        let name = next_ordinal_name(part.name(), &mut used);
                        part.with_name(name)
                    };
                    if let Some(slot) = slots.get_mut(at) {
                        *slot = Some(part);
                    }
                }
            }
        }
    }
    fields.extend(slots.into_iter().flatten());
}

/// One column for `parts`, sibling columns of one name, or `None` when
/// their types cannot be merged.
///
/// Parts of an `exclusive` choice, none of them repeated, merge into a
/// scalar; any others into a `List` of what one occurrence holds.
fn merge_columns(parts: &[Field], exclusive: bool) -> Option<Field> {
    let scalar = exclusive && !parts.iter().any(|f| is_repeated(f) || in_repeat(f));
    // The value one occurrence holds, and whether it may be null.
    let item = |field: &Field| match (scalar, is_repeated(field), field.data_type()) {
        (false, true, DataType::List(item)) => (item.data_type().clone(), item.is_nullable()),
        _ => (field.data_type().clone(), field.is_nullable()),
    };
    let (first, rest) = parts.split_first()?;
    let (mut data_type, mut item_nullable) = item(first);
    for part in rest {
        let (next, nullable) = item(part);
        data_type = unify(&data_type, &next)?;
        item_nullable |= nullable;
    }
    let name = first.name().clone();
    let merged = if scalar {
        Field::new(name, data_type, item_nullable)
    } else {
        // Absent only when every part may be absent.
        let nullable = parts.iter().all(Field::is_nullable);
        let item = Field::new("item", data_type, item_nullable);
        with_mark(
            Field::new(name, DataType::List(Arc::new(item)), nullable),
            REPEATED_MARK,
        )
    };
    // Only columns of the element's own merged count as a merge to the
    // element: a referenced attribute's column it may leave out.
    let own = parts.iter().filter(|f| !is_referenced(f)).count();
    let merged = if own >= 2 {
        with_mark(merged, MERGED_MARK)
    } else {
        merged
    };
    Some(if own == 0 {
        mark_referenced(merged)
    } else {
        merged
    })
}

/// `name#n` for the base of `name` (before any `#`) and the least `n` from
/// 2 not in `used`, which it is added to.
fn next_ordinal_name(name: &str, used: &mut std::collections::HashSet<String>) -> String {
    let base = name.split('#').next().unwrap_or(name);
    let mut ordinal = 2usize;
    loop {
        let candidate = format!("{base}#{ordinal}");
        if used.insert(candidate.clone()) {
            return candidate;
        }
        ordinal += 1;
    }
}

/// The type both `a` and `b` fit: equal up to the nullability of nested
/// fields, which is kept wherever either has it. `None` if they differ
/// otherwise.
fn unify(a: &DataType, b: &DataType) -> Option<DataType> {
    if a == b {
        return Some(a.clone());
    }
    match (a, b) {
        (DataType::Struct(fa), DataType::Struct(fb)) if fa.len() == fb.len() => {
            let mut fields = Vec::with_capacity(fa.len());
            for (x, y) in fa.iter().zip(fb.iter()) {
                if x.name() != y.name() {
                    return None;
                }
                let data_type = unify(x.data_type(), y.data_type())?;
                fields.push(Field::new(
                    x.name(),
                    data_type,
                    x.is_nullable() || y.is_nullable(),
                ));
            }
            Some(DataType::Struct(fields.into()))
        }
        (DataType::List(x), DataType::List(y)) if x.name() == y.name() => {
            let data_type = unify(x.data_type(), y.data_type())?;
            Some(DataType::List(Arc::new(Field::new(
                x.name(),
                data_type,
                x.is_nullable() || y.is_nullable(),
            ))))
        }
        _ => None,
    }
}

/// Marks every field nullable: the group that derived them may be absent.
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

/// The Arrow type of a simple type's value: a list of its items for an
/// `xs:list`, and the type of its kind otherwise.
fn simple_type_to_arrow(simple: &SimpleTypeDef, opts: &ArrowOptions) -> DataType {
    match &simple.list_item {
        Some(item) => DataType::List(Arc::new(Field::new(
            "item",
            simple_kind_to_arrow(&item.kind, opts),
            true,
        ))),
        None => simple_kind_to_arrow(&simple.kind, opts),
    }
}

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

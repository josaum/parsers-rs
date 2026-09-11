use crate::{parse_pptx, ParseOptions, PptxDocument, ShapeText, Table};
use anyhow::{anyhow, Context, Result};
use arrow_array::builder::{
    BooleanBuilder, StringBuilder, UInt32Builder, UInt64Builder, UInt8Builder,
};
use arrow_array::{ArrayRef, RecordBatch};
use arrow_ipc::writer::StreamWriter;
use arrow_schema::{DataType, Field, Schema, SchemaRef};
use std::fs::File;
use std::io::BufWriter;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Options controlling `.pptx` to Arrow shredding and RecordBatch emission.
#[derive(Debug, Clone)]
pub struct ArrowOptions {
    /// Maximum number of rows emitted per [`RecordBatch`].
    pub batch_size: usize,
}

impl Default for ArrowOptions {
    fn default() -> Self {
        Self { batch_size: 16_384 }
    }
}

/// Stable table names emitted by the `.pptx` Arrow exporter.
///
/// Each entry maps to a schema returned by [`schema_for_table`].
pub const TABLES: &[&str] = &[
    "metadata",
    "slides",
    "masters",
    "layouts",
    "shapes",
    "paragraphs",
    "runs",
    "tables",
    "table_rows",
    "table_cells",
    "comments",
    "images",
];

/// Return the stable list of `.pptx` Arrow table names.
pub fn list_tables() -> &'static [&'static str] {
    TABLES
}

/// Return the Arrow schema for a named `.pptx` shredded table.
///
/// Returns `None` when `table` is not one of [`TABLES`].
pub fn schema_for_table(table: &str) -> Option<SchemaRef> {
    match table {
        "metadata" => Some(metadata_schema()),
        "slides" => Some(slides_schema()),
        "masters" => Some(masters_schema()),
        "layouts" => Some(layouts_schema()),
        "shapes" => Some(shapes_schema()),
        "paragraphs" => Some(paragraphs_schema()),
        "runs" => Some(runs_schema()),
        "tables" => Some(tables_schema()),
        "table_rows" => Some(table_rows_schema()),
        "table_cells" => Some(table_cells_schema()),
        "comments" => Some(comments_schema()),
        "images" => Some(images_schema()),
        _ => None,
    }
}

/// Stream one shredded table from an already-parsed [`PptxDocument`].
///
/// Prefer this over re-parsing when the caller already holds the presentation.
/// Unknown `table` names fail loudly — they must match [`TABLES`] /
/// [`schema_for_table`].
pub fn pptx_document_to_arrow_table_batches<F>(
    doc: &PptxDocument,
    arrow_options: ArrowOptions,
    table: &str,
    mut emit: F,
) -> Result<()>
where
    F: FnMut(RecordBatch) -> Result<()>,
{
    let tables = collect_pptx_tables(doc);
    let schema =
        schema_for_table(table).ok_or_else(|| anyhow::anyhow!("unknown pptx table: {table}"))?;
    emit_pptx_table_batches(&tables, table, arrow_options.batch_size, &schema, &mut emit)
}

/// Encode one shredded table from an already-parsed presentation as an
/// in-memory Arrow IPC stream.
pub fn pptx_document_to_arrow_ipc(
    doc: &PptxDocument,
    arrow_options: ArrowOptions,
    table: &str,
) -> Result<Vec<u8>> {
    let schema =
        schema_for_table(table).ok_or_else(|| anyhow::anyhow!("unknown pptx table: {table}"))?;
    let mut ipc = Vec::new();
    {
        let mut writer = StreamWriter::try_new(&mut ipc, &schema)?;
        pptx_document_to_arrow_table_batches(doc, arrow_options, table, |batch| {
            writer.write(&batch)?;
            Ok(())
        })?;
        writer.finish()?;
    }
    Ok(ipc)
}

/// Parse a `.pptx` file and stream batches for one named shredded table.
///
/// This is the one-pass file counterpart to
/// [`pptx_document_to_arrow_table_batches`].
pub fn parse_pptx_to_arrow_table_batches<F>(
    path: impl AsRef<Path>,
    options: ParseOptions,
    arrow_options: ArrowOptions,
    table: &str,
    mut emit: F,
) -> Result<()>
where
    F: FnMut(RecordBatch) -> Result<()>,
{
    let doc = parse_pptx(path, options)?;
    pptx_document_to_arrow_table_batches(&doc, arrow_options, table, &mut emit)
}

/// Parse a `.pptx` file and encode one named shredded table as an in-memory
/// Arrow IPC stream.
///
/// The `_bytes` suffix is load-bearing. A presentation shreds into a whole set
/// of tables, not one, so there is also a directory-shaped entry point
/// ([`parse_pptx_to_arrow_dataset`]); a name that said neither would leave a
/// caller guessing which of the two they were holding. The two differ in both
/// argument count and arity of the result, so they cannot share a name without
/// one of them silently changing meaning.
pub fn parse_pptx_to_arrow_ipc_bytes(
    path: impl AsRef<Path>,
    options: ParseOptions,
    arrow_options: ArrowOptions,
    table: &str,
) -> Result<Vec<u8>> {
    let doc = parse_pptx(path, options)?;
    pptx_document_to_arrow_ipc(&doc, arrow_options, table)
}

/// Parse a `.pptx` file and write every shredded table into `out_dir`, one
/// `<table>.arrow` file per entry of [`TABLES`], returning the paths written.
///
/// This is the whole-presentation counterpart to
/// [`parse_pptx_to_arrow_ipc_bytes`], which encodes a single named table to
/// memory. Use this one when the caller wants the dataset rather than one
/// table.
pub fn parse_pptx_to_arrow_dataset(
    path: impl AsRef<Path>,
    out_dir: impl AsRef<Path>,
    options: ParseOptions,
    arrow_options: ArrowOptions,
) -> Result<Vec<PathBuf>> {
    let doc = parse_pptx(path, options)?;
    pptx_document_to_arrow_dataset(&doc, out_dir, arrow_options)
}

/// Write every shredded table of an already-parsed document into `out_dir`.
///
/// Each file is a complete Arrow IPC stream for that table's schema. A failure
/// part-way leaves the files already written in place: the returned list is
/// what succeeded, and a caller that needs all-or-nothing output should write
/// to a temporary directory and rename.
pub fn pptx_document_to_arrow_dataset(
    doc: &PptxDocument,
    out_dir: impl AsRef<Path>,
    arrow_options: ArrowOptions,
) -> Result<Vec<PathBuf>> {
    let tables = collect_pptx_tables(doc);
    let out_dir = out_dir.as_ref();
    std::fs::create_dir_all(out_dir)
        .with_context(|| format!("create output dir: {}", out_dir.display()))?;

    let mut outputs = Vec::new();
    for &table in TABLES {
        let schema = schema_for_table(table)
            .ok_or_else(|| anyhow!("pptx TABLES entry `{table}` missing schema_for_table arm"))?;
        let out_path = out_dir.join(format!("{table}.arrow"));
        let out = File::create(&out_path)
            .with_context(|| format!("create output: {}", out_path.display()))?;
        let mut writer = StreamWriter::try_new(BufWriter::new(out), &schema)?;
        emit_pptx_table_batches(&tables, table, arrow_options.batch_size, &schema, |batch| {
            writer.write(&batch)?;
            Ok(())
        })?;
        writer.finish()?;
        outputs.push(out_path);
    }

    Ok(outputs)
}

#[derive(Default)]
struct PptxTables {
    metadata: Vec<MetadataRow>,
    slides: Vec<SlideRow>,
    masters: Vec<MasterRow>,
    layouts: Vec<LayoutRow>,
    shapes: Vec<ShapeRow>,
    paragraphs: Vec<ParagraphRow>,
    runs: Vec<RunRow>,
    tables: Vec<TableRow>,
    table_rows: Vec<TableRowRow>,
    table_cells: Vec<TableCellRow>,
    comments: Vec<CommentRow>,
    images: Vec<ImageRow>,
}

#[derive(Default)]
struct IdCounters {
    next_shape: u64,
    next_paragraph: u64,
    next_run: u64,
    next_table: u64,
    next_row: u64,
    next_cell: u64,
}

#[derive(Clone)]
struct MetadataRow {
    section: String,
    key: String,
    value: String,
}

#[derive(Clone)]
struct SlideRow {
    slide_id: u32,
    name: Option<String>,
    slide_index: u32,
}

#[derive(Clone)]
struct MasterRow {
    master_id: String,
    target: String,
    master_index: u32,
}

#[derive(Clone)]
struct LayoutRow {
    layout_id: String,
    target: String,
    layout_index: u32,
}

#[derive(Clone)]
struct ShapeRow {
    shape_id: u64,
    owner_kind: String,
    owner_id: String,
    shape_xml_id: Option<u32>,
    name: Option<String>,
    placeholder: Option<String>,
    shape_index: u32,
}

#[derive(Clone)]
struct ParagraphRow {
    paragraph_id: u64,
    shape_id: u64,
    para_index: u32,
    level: Option<u8>,
    text: String,
}

#[derive(Clone)]
struct RunRow {
    run_id: u64,
    paragraph_id: u64,
    run_index: u32,
    text: String,
    bold: bool,
    italic: bool,
    underline: bool,
}

#[derive(Clone)]
struct TableRow {
    table_id: u64,
    owner_kind: String,
    owner_id: String,
    table_index: u32,
}

#[derive(Clone)]
struct TableRowRow {
    row_id: u64,
    table_id: u64,
    row_index: u32,
}

#[derive(Clone)]
struct TableCellRow {
    cell_id: u64,
    row_id: u64,
    col_index: u32,
    text: String,
}

#[derive(Clone)]
struct CommentRow {
    comment_id: u32,
    author: Option<String>,
    date: Option<String>,
    text: Option<String>,
    slide_id: u32,
}

#[derive(Clone)]
struct ImageRow {
    image_id: String,
    source_part: String,
    target: String,
    content_type: Option<String>,
}

fn collect_pptx_tables(doc: &PptxDocument) -> PptxTables {
    let mut tables = PptxTables::default();
    for (key, value) in &doc.metadata.core {
        tables.metadata.push(MetadataRow {
            section: "core".to_string(),
            key: key.clone(),
            value: value.clone(),
        });
    }
    for (key, value) in &doc.metadata.app {
        tables.metadata.push(MetadataRow {
            section: "app".to_string(),
            key: key.clone(),
            value: value.clone(),
        });
    }
    for (key, value) in &doc.metadata.custom {
        tables.metadata.push(MetadataRow {
            section: "custom".to_string(),
            key: key.clone(),
            value: value.clone(),
        });
    }

    let mut ids = IdCounters::default();

    for (idx, slide) in doc.slides.iter().enumerate() {
        tables.slides.push(SlideRow {
            slide_id: slide.id,
            name: slide.name.clone(),
            slide_index: idx as u32,
        });
        collect_shapes(
            &slide.shapes,
            "slide",
            slide.id.to_string(),
            &mut tables,
            &mut ids,
        );
        collect_shapes(
            &slide.notes,
            "slide_notes",
            slide.id.to_string(),
            &mut tables,
            &mut ids,
        );
        collect_tables(
            &slide.tables,
            "slide",
            slide.id.to_string(),
            &mut tables,
            &mut ids,
        );
    }

    for (idx, master) in doc.masters.iter().enumerate() {
        tables.masters.push(MasterRow {
            master_id: master.id.clone(),
            target: master.target.clone(),
            master_index: idx as u32,
        });
        collect_shapes(
            &master.shapes,
            "master",
            master.id.clone(),
            &mut tables,
            &mut ids,
        );
        collect_tables(
            &master.tables,
            "master",
            master.id.clone(),
            &mut tables,
            &mut ids,
        );
    }

    for (idx, layout) in doc.layouts.iter().enumerate() {
        tables.layouts.push(LayoutRow {
            layout_id: layout.id.clone(),
            target: layout.target.clone(),
            layout_index: idx as u32,
        });
        collect_shapes(
            &layout.shapes,
            "layout",
            layout.id.clone(),
            &mut tables,
            &mut ids,
        );
        collect_tables(
            &layout.tables,
            "layout",
            layout.id.clone(),
            &mut tables,
            &mut ids,
        );
    }

    for comment in &doc.comments {
        tables.comments.push(CommentRow {
            comment_id: comment.id,
            author: comment.author.clone(),
            date: comment.date.clone(),
            text: comment.text.clone(),
            slide_id: comment.slide_id,
        });
    }

    for image in &doc.images {
        tables.images.push(ImageRow {
            image_id: image.id.clone(),
            source_part: image.source_part.clone(),
            target: image.target.clone(),
            content_type: image.content_type.clone(),
        });
    }

    tables
}

fn collect_shapes(
    shapes: &[ShapeText],
    owner_kind: &str,
    owner_id: String,
    tables: &mut PptxTables,
    ids: &mut IdCounters,
) {
    for (shape_index, shape) in shapes.iter().enumerate() {
        let shape_id = ids.next_shape;
        ids.next_shape += 1;
        tables.shapes.push(ShapeRow {
            shape_id,
            owner_kind: owner_kind.to_string(),
            owner_id: owner_id.clone(),
            shape_xml_id: shape.shape_id,
            name: shape.name.clone(),
            placeholder: shape.placeholder.clone(),
            shape_index: shape_index as u32,
        });
        for (para_index, para) in shape.paragraphs.iter().enumerate() {
            let paragraph_id = ids.next_paragraph;
            ids.next_paragraph += 1;
            tables.paragraphs.push(ParagraphRow {
                paragraph_id,
                shape_id,
                para_index: para_index as u32,
                level: para.level,
                text: para.text.clone(),
            });
            for (run_index, run) in para.runs.iter().enumerate() {
                let run_id = ids.next_run;
                ids.next_run += 1;
                tables.runs.push(RunRow {
                    run_id,
                    paragraph_id,
                    run_index: run_index as u32,
                    text: run.text.clone(),
                    bold: run.bold,
                    italic: run.italic,
                    underline: run.underline,
                });
            }
        }
    }
}

fn collect_tables(
    tables_in: &[Table],
    owner_kind: &str,
    owner_id: String,
    tables: &mut PptxTables,
    ids: &mut IdCounters,
) {
    for (table_index, table) in tables_in.iter().enumerate() {
        let table_id = ids.next_table;
        ids.next_table += 1;
        tables.tables.push(TableRow {
            table_id,
            owner_kind: owner_kind.to_string(),
            owner_id: owner_id.clone(),
            table_index: table_index as u32,
        });
        for (row_index, row) in table.rows.iter().enumerate() {
            let row_id = ids.next_row;
            ids.next_row += 1;
            tables.table_rows.push(TableRowRow {
                row_id,
                table_id,
                row_index: row_index as u32,
            });
            for (col_index, cell) in row.cells.iter().enumerate() {
                let cell_id = ids.next_cell;
                ids.next_cell += 1;
                tables.table_cells.push(TableCellRow {
                    cell_id,
                    row_id,
                    col_index: col_index as u32,
                    text: cell.text.clone(),
                });
            }
        }
    }
}

fn metadata_schema() -> SchemaRef {
    Arc::new(Schema::new(vec![
        Field::new("section", DataType::Utf8, false),
        Field::new("key", DataType::Utf8, false),
        Field::new("value", DataType::Utf8, false),
    ]))
}

fn slides_schema() -> SchemaRef {
    Arc::new(Schema::new(vec![
        Field::new("slide_id", DataType::UInt32, false),
        Field::new("name", DataType::Utf8, true),
        Field::new("slide_index", DataType::UInt32, false),
    ]))
}

fn masters_schema() -> SchemaRef {
    Arc::new(Schema::new(vec![
        Field::new("master_id", DataType::Utf8, false),
        Field::new("target", DataType::Utf8, false),
        Field::new("master_index", DataType::UInt32, false),
    ]))
}

fn layouts_schema() -> SchemaRef {
    Arc::new(Schema::new(vec![
        Field::new("layout_id", DataType::Utf8, false),
        Field::new("target", DataType::Utf8, false),
        Field::new("layout_index", DataType::UInt32, false),
    ]))
}

fn shapes_schema() -> SchemaRef {
    Arc::new(Schema::new(vec![
        Field::new("shape_id", DataType::UInt64, false),
        Field::new("owner_kind", DataType::Utf8, false),
        Field::new("owner_id", DataType::Utf8, false),
        Field::new("shape_xml_id", DataType::UInt32, true),
        Field::new("name", DataType::Utf8, true),
        Field::new("placeholder", DataType::Utf8, true),
        Field::new("shape_index", DataType::UInt32, false),
    ]))
}

fn paragraphs_schema() -> SchemaRef {
    Arc::new(Schema::new(vec![
        Field::new("paragraph_id", DataType::UInt64, false),
        Field::new("shape_id", DataType::UInt64, false),
        Field::new("para_index", DataType::UInt32, false),
        Field::new("level", DataType::UInt8, true),
        Field::new("text", DataType::Utf8, false),
    ]))
}

fn runs_schema() -> SchemaRef {
    Arc::new(Schema::new(vec![
        Field::new("run_id", DataType::UInt64, false),
        Field::new("paragraph_id", DataType::UInt64, false),
        Field::new("run_index", DataType::UInt32, false),
        Field::new("text", DataType::Utf8, false),
        Field::new("bold", DataType::Boolean, false),
        Field::new("italic", DataType::Boolean, false),
        Field::new("underline", DataType::Boolean, false),
    ]))
}

fn tables_schema() -> SchemaRef {
    Arc::new(Schema::new(vec![
        Field::new("table_id", DataType::UInt64, false),
        Field::new("owner_kind", DataType::Utf8, false),
        Field::new("owner_id", DataType::Utf8, false),
        Field::new("table_index", DataType::UInt32, false),
    ]))
}

fn table_rows_schema() -> SchemaRef {
    Arc::new(Schema::new(vec![
        Field::new("row_id", DataType::UInt64, false),
        Field::new("table_id", DataType::UInt64, false),
        Field::new("row_index", DataType::UInt32, false),
    ]))
}

fn table_cells_schema() -> SchemaRef {
    Arc::new(Schema::new(vec![
        Field::new("cell_id", DataType::UInt64, false),
        Field::new("row_id", DataType::UInt64, false),
        Field::new("col_index", DataType::UInt32, false),
        Field::new("text", DataType::Utf8, false),
    ]))
}

fn comments_schema() -> SchemaRef {
    Arc::new(Schema::new(vec![
        Field::new("comment_id", DataType::UInt32, false),
        Field::new("author", DataType::Utf8, true),
        Field::new("date", DataType::Utf8, true),
        Field::new("text", DataType::Utf8, true),
        Field::new("slide_id", DataType::UInt32, false),
    ]))
}

fn images_schema() -> SchemaRef {
    Arc::new(Schema::new(vec![
        Field::new("image_id", DataType::Utf8, false),
        Field::new("source_part", DataType::Utf8, false),
        Field::new("target", DataType::Utf8, false),
        Field::new("content_type", DataType::Utf8, true),
    ]))
}

fn emit_pptx_table_batches<F>(
    tables: &PptxTables,
    table: &str,
    batch_size: usize,
    schema: &SchemaRef,
    mut emit: F,
) -> Result<()>
where
    F: FnMut(RecordBatch) -> Result<()>,
{
    match table {
        "metadata" => emit_metadata_batches(&tables.metadata, batch_size, schema, &mut emit),
        "slides" => emit_slides_batches(&tables.slides, batch_size, schema, &mut emit),
        "masters" => emit_masters_batches(&tables.masters, batch_size, schema, &mut emit),
        "layouts" => emit_layouts_batches(&tables.layouts, batch_size, schema, &mut emit),
        "shapes" => emit_shapes_batches(&tables.shapes, batch_size, schema, &mut emit),
        "paragraphs" => emit_paragraphs_batches(&tables.paragraphs, batch_size, schema, &mut emit),
        "runs" => emit_runs_batches(&tables.runs, batch_size, schema, &mut emit),
        "tables" => emit_tables_batches(&tables.tables, batch_size, schema, &mut emit),
        "table_rows" => emit_table_rows_batches(&tables.table_rows, batch_size, schema, &mut emit),
        "table_cells" => {
            emit_table_cells_batches(&tables.table_cells, batch_size, schema, &mut emit)
        }
        "comments" => emit_comments_batches(&tables.comments, batch_size, schema, &mut emit),
        "images" => emit_images_batches(&tables.images, batch_size, schema, &mut emit),
        _ => Err(anyhow::anyhow!("unknown pptx table: {table}")),
    }
}

fn emit_metadata_batches<F>(
    rows: &[MetadataRow],
    batch_size: usize,
    schema: &SchemaRef,
    emit: &mut F,
) -> Result<()>
where
    F: FnMut(RecordBatch) -> Result<()>,
{
    for chunk in rows.chunks(batch_size) {
        let mut section = StringBuilder::new();
        let mut key = StringBuilder::new();
        let mut value = StringBuilder::new();
        for row in chunk {
            section.append_value(&row.section);
            key.append_value(&row.key);
            value.append_value(&row.value);
        }
        let batch = RecordBatch::try_new(
            schema.clone(),
            vec![
                Arc::new(section.finish()) as ArrayRef,
                Arc::new(key.finish()),
                Arc::new(value.finish()),
            ],
        )?;
        emit(batch)?;
    }
    Ok(())
}

fn emit_slides_batches<F>(
    rows: &[SlideRow],
    batch_size: usize,
    schema: &SchemaRef,
    emit: &mut F,
) -> Result<()>
where
    F: FnMut(RecordBatch) -> Result<()>,
{
    for chunk in rows.chunks(batch_size) {
        let mut slide_id = UInt32Builder::new();
        let mut name = StringBuilder::new();
        let mut slide_index = UInt32Builder::new();
        for row in chunk {
            slide_id.append_value(row.slide_id);
            name.append_option(row.name.as_deref());
            slide_index.append_value(row.slide_index);
        }
        let batch = RecordBatch::try_new(
            schema.clone(),
            vec![
                Arc::new(slide_id.finish()),
                Arc::new(name.finish()),
                Arc::new(slide_index.finish()),
            ],
        )?;
        emit(batch)?;
    }
    Ok(())
}

fn emit_masters_batches<F>(
    rows: &[MasterRow],
    batch_size: usize,
    schema: &SchemaRef,
    emit: &mut F,
) -> Result<()>
where
    F: FnMut(RecordBatch) -> Result<()>,
{
    for chunk in rows.chunks(batch_size) {
        let mut master_id = StringBuilder::new();
        let mut target = StringBuilder::new();
        let mut master_index = UInt32Builder::new();
        for row in chunk {
            master_id.append_value(&row.master_id);
            target.append_value(&row.target);
            master_index.append_value(row.master_index);
        }
        let batch = RecordBatch::try_new(
            schema.clone(),
            vec![
                Arc::new(master_id.finish()),
                Arc::new(target.finish()),
                Arc::new(master_index.finish()),
            ],
        )?;
        emit(batch)?;
    }
    Ok(())
}

fn emit_layouts_batches<F>(
    rows: &[LayoutRow],
    batch_size: usize,
    schema: &SchemaRef,
    emit: &mut F,
) -> Result<()>
where
    F: FnMut(RecordBatch) -> Result<()>,
{
    for chunk in rows.chunks(batch_size) {
        let mut layout_id = StringBuilder::new();
        let mut target = StringBuilder::new();
        let mut layout_index = UInt32Builder::new();
        for row in chunk {
            layout_id.append_value(&row.layout_id);
            target.append_value(&row.target);
            layout_index.append_value(row.layout_index);
        }
        let batch = RecordBatch::try_new(
            schema.clone(),
            vec![
                Arc::new(layout_id.finish()),
                Arc::new(target.finish()),
                Arc::new(layout_index.finish()),
            ],
        )?;
        emit(batch)?;
    }
    Ok(())
}

fn emit_shapes_batches<F>(
    rows: &[ShapeRow],
    batch_size: usize,
    schema: &SchemaRef,
    emit: &mut F,
) -> Result<()>
where
    F: FnMut(RecordBatch) -> Result<()>,
{
    for chunk in rows.chunks(batch_size) {
        let mut shape_id = UInt64Builder::new();
        let mut owner_kind = StringBuilder::new();
        let mut owner_id = StringBuilder::new();
        let mut shape_xml_id = UInt32Builder::new();
        let mut name = StringBuilder::new();
        let mut placeholder = StringBuilder::new();
        let mut shape_index = UInt32Builder::new();
        for row in chunk {
            shape_id.append_value(row.shape_id);
            owner_kind.append_value(&row.owner_kind);
            owner_id.append_value(&row.owner_id);
            shape_xml_id.append_option(row.shape_xml_id);
            name.append_option(row.name.as_deref());
            placeholder.append_option(row.placeholder.as_deref());
            shape_index.append_value(row.shape_index);
        }
        let batch = RecordBatch::try_new(
            schema.clone(),
            vec![
                Arc::new(shape_id.finish()),
                Arc::new(owner_kind.finish()),
                Arc::new(owner_id.finish()),
                Arc::new(shape_xml_id.finish()),
                Arc::new(name.finish()),
                Arc::new(placeholder.finish()),
                Arc::new(shape_index.finish()),
            ],
        )?;
        emit(batch)?;
    }
    Ok(())
}

fn emit_paragraphs_batches<F>(
    rows: &[ParagraphRow],
    batch_size: usize,
    schema: &SchemaRef,
    emit: &mut F,
) -> Result<()>
where
    F: FnMut(RecordBatch) -> Result<()>,
{
    for chunk in rows.chunks(batch_size) {
        let mut paragraph_id = UInt64Builder::new();
        let mut shape_id = UInt64Builder::new();
        let mut para_index = UInt32Builder::new();
        let mut level = UInt8Builder::new();
        let mut text = StringBuilder::new();
        for row in chunk {
            paragraph_id.append_value(row.paragraph_id);
            shape_id.append_value(row.shape_id);
            para_index.append_value(row.para_index);
            level.append_option(row.level);
            text.append_value(&row.text);
        }
        let batch = RecordBatch::try_new(
            schema.clone(),
            vec![
                Arc::new(paragraph_id.finish()),
                Arc::new(shape_id.finish()),
                Arc::new(para_index.finish()),
                Arc::new(level.finish()),
                Arc::new(text.finish()),
            ],
        )?;
        emit(batch)?;
    }
    Ok(())
}

fn emit_runs_batches<F>(
    rows: &[RunRow],
    batch_size: usize,
    schema: &SchemaRef,
    emit: &mut F,
) -> Result<()>
where
    F: FnMut(RecordBatch) -> Result<()>,
{
    for chunk in rows.chunks(batch_size) {
        let mut run_id = UInt64Builder::new();
        let mut paragraph_id = UInt64Builder::new();
        let mut run_index = UInt32Builder::new();
        let mut text = StringBuilder::new();
        let mut bold = BooleanBuilder::new();
        let mut italic = BooleanBuilder::new();
        let mut underline = BooleanBuilder::new();
        for row in chunk {
            run_id.append_value(row.run_id);
            paragraph_id.append_value(row.paragraph_id);
            run_index.append_value(row.run_index);
            text.append_value(&row.text);
            bold.append_value(row.bold);
            italic.append_value(row.italic);
            underline.append_value(row.underline);
        }
        let batch = RecordBatch::try_new(
            schema.clone(),
            vec![
                Arc::new(run_id.finish()),
                Arc::new(paragraph_id.finish()),
                Arc::new(run_index.finish()),
                Arc::new(text.finish()),
                Arc::new(bold.finish()),
                Arc::new(italic.finish()),
                Arc::new(underline.finish()),
            ],
        )?;
        emit(batch)?;
    }
    Ok(())
}

fn emit_tables_batches<F>(
    rows: &[TableRow],
    batch_size: usize,
    schema: &SchemaRef,
    emit: &mut F,
) -> Result<()>
where
    F: FnMut(RecordBatch) -> Result<()>,
{
    for chunk in rows.chunks(batch_size) {
        let mut table_id = UInt64Builder::new();
        let mut owner_kind = StringBuilder::new();
        let mut owner_id = StringBuilder::new();
        let mut table_index = UInt32Builder::new();
        for row in chunk {
            table_id.append_value(row.table_id);
            owner_kind.append_value(&row.owner_kind);
            owner_id.append_value(&row.owner_id);
            table_index.append_value(row.table_index);
        }
        let batch = RecordBatch::try_new(
            schema.clone(),
            vec![
                Arc::new(table_id.finish()),
                Arc::new(owner_kind.finish()),
                Arc::new(owner_id.finish()),
                Arc::new(table_index.finish()),
            ],
        )?;
        emit(batch)?;
    }
    Ok(())
}

fn emit_table_rows_batches<F>(
    rows: &[TableRowRow],
    batch_size: usize,
    schema: &SchemaRef,
    emit: &mut F,
) -> Result<()>
where
    F: FnMut(RecordBatch) -> Result<()>,
{
    for chunk in rows.chunks(batch_size) {
        let mut row_id = UInt64Builder::new();
        let mut table_id = UInt64Builder::new();
        let mut row_index = UInt32Builder::new();
        for row in chunk {
            row_id.append_value(row.row_id);
            table_id.append_value(row.table_id);
            row_index.append_value(row.row_index);
        }
        let batch = RecordBatch::try_new(
            schema.clone(),
            vec![
                Arc::new(row_id.finish()),
                Arc::new(table_id.finish()),
                Arc::new(row_index.finish()),
            ],
        )?;
        emit(batch)?;
    }
    Ok(())
}

fn emit_table_cells_batches<F>(
    rows: &[TableCellRow],
    batch_size: usize,
    schema: &SchemaRef,
    emit: &mut F,
) -> Result<()>
where
    F: FnMut(RecordBatch) -> Result<()>,
{
    for chunk in rows.chunks(batch_size) {
        let mut cell_id = UInt64Builder::new();
        let mut row_id = UInt64Builder::new();
        let mut col_index = UInt32Builder::new();
        let mut text = StringBuilder::new();
        for row in chunk {
            cell_id.append_value(row.cell_id);
            row_id.append_value(row.row_id);
            col_index.append_value(row.col_index);
            text.append_value(&row.text);
        }
        let batch = RecordBatch::try_new(
            schema.clone(),
            vec![
                Arc::new(cell_id.finish()),
                Arc::new(row_id.finish()),
                Arc::new(col_index.finish()),
                Arc::new(text.finish()),
            ],
        )?;
        emit(batch)?;
    }
    Ok(())
}

fn emit_comments_batches<F>(
    rows: &[CommentRow],
    batch_size: usize,
    schema: &SchemaRef,
    emit: &mut F,
) -> Result<()>
where
    F: FnMut(RecordBatch) -> Result<()>,
{
    for chunk in rows.chunks(batch_size) {
        let mut comment_id = UInt32Builder::new();
        let mut author = StringBuilder::new();
        let mut date = StringBuilder::new();
        let mut text = StringBuilder::new();
        let mut slide_id = UInt32Builder::new();
        for row in chunk {
            comment_id.append_value(row.comment_id);
            author.append_option(row.author.as_deref());
            date.append_option(row.date.as_deref());
            text.append_option(row.text.as_deref());
            slide_id.append_value(row.slide_id);
        }
        let batch = RecordBatch::try_new(
            schema.clone(),
            vec![
                Arc::new(comment_id.finish()),
                Arc::new(author.finish()),
                Arc::new(date.finish()),
                Arc::new(text.finish()),
                Arc::new(slide_id.finish()),
            ],
        )?;
        emit(batch)?;
    }
    Ok(())
}

fn emit_images_batches<F>(
    rows: &[ImageRow],
    batch_size: usize,
    schema: &SchemaRef,
    emit: &mut F,
) -> Result<()>
where
    F: FnMut(RecordBatch) -> Result<()>,
{
    for chunk in rows.chunks(batch_size) {
        let mut image_id = StringBuilder::new();
        let mut source_part = StringBuilder::new();
        let mut target = StringBuilder::new();
        let mut content_type = StringBuilder::new();
        for row in chunk {
            image_id.append_value(&row.image_id);
            source_part.append_value(&row.source_part);
            target.append_value(&row.target);
            content_type.append_option(row.content_type.as_deref());
        }
        let batch = RecordBatch::try_new(
            schema.clone(),
            vec![
                Arc::new(image_id.finish()),
                Arc::new(source_part.finish()),
                Arc::new(target.finish()),
                Arc::new(content_type.finish()),
            ],
        )?;
        emit(batch)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Metadata, Slide};
    use std::collections::HashMap;

    fn sample_doc() -> PptxDocument {
        PptxDocument {
            metadata: Metadata {
                core: HashMap::new(),
                app: HashMap::new(),
                custom: HashMap::new(),
            },
            slides: vec![Slide {
                id: 1,
                name: Some("Intro".into()),
                shapes: vec![],
                tables: vec![],
                notes: vec![],
            }],
            masters: vec![],
            layouts: vec![],
            comments: vec![],
            images: vec![],
        }
    }

    #[test]
    fn tables_and_schema_for_table_stay_in_lockstep() {
        // WHY: table selection relies on TABLES and schema lookup agreeing.
        for &table in TABLES {
            assert!(
                schema_for_table(table).is_some(),
                "missing schema_for_table arm for `{table}`"
            );
        }
    }

    #[test]
    fn unknown_table_name_fails_loudly() {
        let err = pptx_document_to_arrow_table_batches(
            &sample_doc(),
            ArrowOptions::default(),
            "not_a_real_table",
            |_| Ok(()),
        )
        .expect_err("unknown table must error");
        assert!(err.to_string().contains("unknown pptx table"));
    }

    #[test]
    fn in_memory_slides_batch_has_rows() {
        // WHY: Flight/ETL should shred an already-parsed PptxDocument without re-parse.
        let mut rows = 0usize;
        pptx_document_to_arrow_table_batches(
            &sample_doc(),
            ArrowOptions { batch_size: 1024 },
            "slides",
            |batch| {
                rows += batch.num_rows();
                Ok(())
            },
        )
        .expect("slides emit");
        assert_eq!(rows, 1);
    }

    #[test]
    fn in_memory_slides_ipc_is_readable() {
        use arrow_ipc::reader::StreamReader;
        use std::io::Cursor;

        let ipc = pptx_document_to_arrow_ipc(
            &sample_doc(),
            ArrowOptions { batch_size: 1024 },
            "slides",
        )
        .expect("encode slides IPC");
        let rows = StreamReader::try_new(Cursor::new(ipc), None)
            .expect("read slides IPC")
            .map(|batch| batch.expect("valid batch").num_rows())
            .sum::<usize>();
        assert_eq!(rows, 1);
    }

    /// The dataset writer must produce one readable IPC file per declared
    /// table, and each file's schema must be the one `schema_for_table` claims
    /// — the whole point of writing them separately.
    #[test]
    fn the_dataset_writer_emits_one_readable_file_per_table() {
        use arrow_ipc::reader::StreamReader;
        use std::fs::File;

        let dir = tempfile::tempdir().expect("temp dir");
        let written = pptx_document_to_arrow_dataset(
            &sample_doc(),
            dir.path(),
            ArrowOptions { batch_size: 1024 },
        )
        .expect("write pptx dataset");

        assert_eq!(
            written.len(),
            TABLES.len(),
            "one output file per declared table"
        );
        for (table, path) in TABLES.iter().zip(&written) {
            let expected = format!("{table}.arrow");
            assert_eq!(
                path.file_name().and_then(|n| n.to_str()),
                Some(expected.as_str())
            );
            let file = File::open(path).unwrap_or_else(|e| panic!("open {table}: {e}"));
            let reader = StreamReader::try_new(std::io::BufReader::new(file), None)
                .unwrap_or_else(|e| panic!("{table} is not a readable IPC stream: {e}"));
            assert_eq!(
                reader.schema().as_ref(),
                schema_for_table(table).unwrap().as_ref(),
                "{table} on disk does not carry its declared schema"
            );
        }
    }
}

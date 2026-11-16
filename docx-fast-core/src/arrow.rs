use crate::{parse_docx, Block, DocxDocument, ParseOptions};
use anyhow::{Context, Result};
use arrow_array::builder::{
    BooleanBuilder, Int32Builder, Int64Builder, ListBuilder, StringBuilder, UInt32Builder,
    UInt64Builder, UInt8Builder,
};
use arrow_array::{ArrayRef, RecordBatch};
use arrow_ipc::writer::StreamWriter;
use arrow_schema::{DataType, Field, Schema, SchemaRef};
use std::fs::File;
use std::io::BufWriter;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Options controlling `.docx` → Arrow shredding and IPC emission.
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

/// Stable table names emitted by the `.docx` Arrow exporter.
///
/// Each entry maps to a schema returned by [`schema_for_table`]. These names are
/// also used as output file stems (`<table>.arrow`) by
/// [`parse_docx_to_arrow_dataset`] and [`docx_document_to_arrow_dataset`].
pub const TABLES: &[&str] = &[
    "metadata",
    "settings",
    "page_count",
    "toc",
    "styles",
    "numbering_levels",
    "numbering_nums",
    "notes",
    "comments",
    "revisions",
    "fields",
    "headers",
    "footers",
    "images",
    "paragraphs",
    "runs",
    "tables",
    "table_rows",
    "table_cells",
];

/// Return the stable list of `.docx` Arrow table names.
pub fn list_tables() -> &'static [&'static str] {
    TABLES
}

/// Return the Arrow schema for a named `.docx` shredded table.
///
/// Returns `None` when `table` is not one of [`TABLES`].
pub fn schema_for_table(table: &str) -> Option<SchemaRef> {
    match table {
        "metadata" => Some(metadata_schema()),
        "settings" => Some(settings_schema()),
        "page_count" => Some(page_count_schema()),
        "toc" => Some(toc_schema()),
        "styles" => Some(styles_schema()),
        "numbering_levels" => Some(numbering_levels_schema()),
        "numbering_nums" => Some(numbering_nums_schema()),
        "notes" => Some(notes_schema()),
        "comments" => Some(comments_schema()),
        "revisions" => Some(revisions_schema()),
        "fields" => Some(fields_schema()),
        "headers" => Some(headers_schema()),
        "footers" => Some(footers_schema()),
        "images" => Some(images_schema()),
        "paragraphs" => Some(paragraphs_schema()),
        "runs" => Some(runs_schema()),
        "tables" => Some(tables_schema()),
        "table_rows" => Some(table_rows_schema()),
        "table_cells" => Some(table_cells_schema()),
        _ => None,
    }
}

/// Parse a `.docx` file and write every shredded table as an Arrow IPC stream.
///
/// The returned paths point to `<out_dir>/<table>.arrow` files, one per entry in
/// [`TABLES`].
pub fn parse_docx_to_arrow_dataset(
    path: impl AsRef<Path>,
    out_dir: impl AsRef<Path>,
    options: ParseOptions,
    arrow_options: ArrowOptions,
) -> Result<Vec<PathBuf>> {
    let doc = parse_docx(path, options)?;
    docx_document_to_arrow_dataset(&doc, out_dir, arrow_options)
}

/// Stream one shredded table from an already-parsed [`DocxDocument`].
///
/// Prefer this over re-parsing when the caller already holds the document
/// (Flight handlers, in-memory ETL). Unknown `table` names fail loudly —
/// they must match [`TABLES`] / [`schema_for_table`].
pub fn docx_document_to_arrow_table_batches<F>(
    doc: &DocxDocument,
    arrow_options: ArrowOptions,
    table: &str,
    mut emit: F,
) -> Result<()>
where
    F: FnMut(RecordBatch) -> Result<()>,
{
    let tables = collect_docx_tables(doc);
    let schema =
        schema_for_table(table).ok_or_else(|| anyhow::anyhow!("unknown docx table: {table}"))?;
    emit_docx_table_batches(&tables, table, arrow_options.batch_size, &schema, &mut emit)
}

/// Write every shredded table for an already-parsed document as Arrow IPC streams.
pub fn docx_document_to_arrow_dataset(
    doc: &DocxDocument,
    out_dir: impl AsRef<Path>,
    arrow_options: ArrowOptions,
) -> Result<Vec<PathBuf>> {
    let tables = collect_docx_tables(doc);
    let out_dir = out_dir.as_ref();
    std::fs::create_dir_all(out_dir)
        .with_context(|| format!("create output dir: {}", out_dir.display()))?;

    let mut outputs = Vec::new();
    for &table in TABLES {
        // TABLES and schema_for_table must stay in lockstep — fail loud if drifted.
        let schema = schema_for_table(table).ok_or_else(|| {
            anyhow::anyhow!("docx TABLES entry `{table}` missing schema_for_table arm")
        })?;
        let out_path = out_dir.join(format!("{table}.arrow"));
        let out = File::create(&out_path)
            .with_context(|| format!("create output: {}", out_path.display()))?;
        let mut writer = StreamWriter::try_new(BufWriter::new(out), &schema)?;
        emit_docx_table_batches(&tables, table, arrow_options.batch_size, &schema, |batch| {
            writer.write(&batch)?;
            Ok(())
        })?;
        writer.finish()?;
        outputs.push(out_path);
    }

    Ok(outputs)
}

/// Parse a `.docx` file and stream batches for one named shredded table.
///
/// This is the one-pass file counterpart to
/// [`docx_document_to_arrow_table_batches`].
pub fn parse_docx_to_arrow_table_batches<F>(
    path: impl AsRef<Path>,
    options: ParseOptions,
    arrow_options: ArrowOptions,
    table: &str,
    mut emit: F,
) -> Result<()>
where
    F: FnMut(RecordBatch) -> Result<()>,
{
    let doc = parse_docx(path, options)?;
    docx_document_to_arrow_table_batches(&doc, arrow_options, table, &mut emit)
}

/// Parse a `.docx` file and write the complete Arrow IPC dataset.
///
/// Compatibility wrapper around [`parse_docx_to_arrow_dataset`] for callers
/// that only need success/failure and not the produced file paths.
pub fn parse_docx_to_arrow_ipc(
    path: impl AsRef<Path>,
    out_dir: impl AsRef<Path>,
    options: ParseOptions,
    arrow_options: ArrowOptions,
) -> Result<()> {
    parse_docx_to_arrow_dataset(path, out_dir, options, arrow_options).map(|_| ())
}

#[derive(Default)]
struct DocxTables {
    metadata: Vec<MetadataRow>,
    settings: Vec<SettingRow>,
    page_count: Option<u32>,
    toc: Vec<TocRow>,
    styles: Vec<StyleRow>,
    numbering_levels: Vec<NumberingLevelRow>,
    numbering_nums: Vec<NumberingNumRow>,
    notes: Vec<NoteRow>,
    comments: Vec<CommentRow>,
    revisions: Vec<RevisionRow>,
    fields: Vec<FieldRow>,
    headers: Vec<HeaderFooterRow>,
    footers: Vec<HeaderFooterRow>,
    images: Vec<ImageRow>,
    paragraphs: Vec<ParagraphRow>,
    runs: Vec<RunRow>,
    tables: Vec<TableRow>,
    table_rows: Vec<TableRowRow>,
    table_cells: Vec<TableCellRow>,
}

#[derive(Default)]
struct IdCounters {
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
struct SettingRow {
    key: String,
    value: String,
}

#[derive(Clone)]
struct TocRow {
    toc_index: u32,
    level: u8,
    text: String,
    source: String,
    paragraph_id: Option<String>,
    style_id: Option<String>,
}

#[derive(Clone)]
struct StyleRow {
    id: String,
    name: Option<String>,
    style_type: Option<String>,
    based_on: Option<String>,
    next: Option<String>,
    outline_level: Option<u8>,
    ui_priority: Option<u8>,
    is_default: bool,
    is_qformat: bool,
}

#[derive(Clone)]
struct NumberingLevelRow {
    abstract_id: u32,
    level: u8,
    format: Option<String>,
    text: Option<String>,
    start: Option<u32>,
}

#[derive(Clone)]
struct NumberingNumRow {
    num_id: u32,
    abstract_id: Option<u32>,
}

#[derive(Clone)]
struct NoteRow {
    note_type: String,
    note_id: i32,
}

#[derive(Clone)]
struct CommentRow {
    comment_id: i32,
    author: Option<String>,
    initials: Option<String>,
    date: Option<String>,
    parent_id: Option<i32>,
}

#[derive(Clone)]
struct RevisionRow {
    revision_id: String,
    kind: String,
    author: Option<String>,
    date: Option<String>,
}

#[derive(Clone)]
struct FieldRow {
    field_id: u32,
    instr: Option<String>,
    result: String,
    kind: Option<String>,
}

#[derive(Clone)]
struct HeaderFooterRow {
    header_id: u32,
    kind: String,
    ref_type: Option<String>,
    part: String,
}

#[derive(Clone)]
struct ImageRow {
    image_id: String,
    source_part: String,
    target: String,
    content_type: Option<String>,
}

#[derive(Clone)]
struct ParagraphRow {
    paragraph_id: u64,
    owner_kind: String,
    owner_id: i64,
    block_index: u32,
    para_xml_id: Option<String>,
    style_id: Option<String>,
    style_name: Option<String>,
    outline_level: Option<u8>,
    num_id: Option<u32>,
    num_level: Option<u8>,
    toc_level: Option<u8>,
    text: String,
}

#[derive(Clone)]
struct RunRow {
    run_id: u64,
    paragraph_id: u64,
    run_index: u32,
    text: String,
    style_id: Option<String>,
    bold: bool,
    italic: bool,
    underline: bool,
    field_id: Option<u32>,
    revision_id: Option<String>,
    revision_kind: Option<String>,
    comment_ids: Vec<i32>,
    footnote_ids: Vec<i32>,
    endnote_ids: Vec<i32>,
}

#[derive(Clone)]
struct TableRow {
    table_id: u64,
    owner_kind: String,
    owner_id: i64,
    block_index: u32,
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
}

fn collect_docx_tables(doc: &DocxDocument) -> DocxTables {
    let mut tables = DocxTables::default();
    for (key, value) in crate::sorted_entries(&doc.metadata.core) {
        tables.metadata.push(MetadataRow {
            section: "core".to_string(),
            key: key.clone(),
            value: value.clone(),
        });
    }
    for (key, value) in crate::sorted_entries(&doc.metadata.app) {
        tables.metadata.push(MetadataRow {
            section: "app".to_string(),
            key: key.clone(),
            value: value.clone(),
        });
    }
    for (key, value) in crate::sorted_entries(&doc.metadata.custom) {
        tables.metadata.push(MetadataRow {
            section: "custom".to_string(),
            key: key.clone(),
            value: value.clone(),
        });
    }

    for (key, value) in crate::sorted_entries(&doc.settings) {
        tables.settings.push(SettingRow {
            key: key.clone(),
            value: value.clone(),
        });
    }

    tables.page_count = doc.page_count;

    for (idx, entry) in doc.toc.iter().enumerate() {
        tables.toc.push(TocRow {
            toc_index: idx as u32,
            level: entry.level,
            text: entry.text.clone(),
            source: entry.source.clone(),
            paragraph_id: entry.paragraph_id.clone(),
            style_id: entry.style_id.clone(),
        });
    }

    for style in &doc.styles {
        tables.styles.push(StyleRow {
            id: style.id.clone(),
            name: style.name.clone(),
            style_type: style.style_type.clone(),
            based_on: style.based_on.clone(),
            next: style.next.clone(),
            outline_level: style.outline_level,
            ui_priority: style.ui_priority,
            is_default: style.is_default,
            is_qformat: style.is_qformat,
        });
    }

    for abstract_num in &doc.numbering.abstract_nums {
        for level in &abstract_num.levels {
            tables.numbering_levels.push(NumberingLevelRow {
                abstract_id: abstract_num.id,
                level: level.level,
                format: level.format.clone(),
                text: level.text.clone(),
                start: level.start,
            });
        }
    }
    for num in &doc.numbering.nums {
        tables.numbering_nums.push(NumberingNumRow {
            num_id: num.id,
            abstract_id: num.abstract_id,
        });
    }

    for note in &doc.footnotes {
        tables.notes.push(NoteRow {
            note_type: "footnote".to_string(),
            note_id: note.id,
        });
    }
    for note in &doc.endnotes {
        tables.notes.push(NoteRow {
            note_type: "endnote".to_string(),
            note_id: note.id,
        });
    }

    for comment in &doc.comments {
        tables.comments.push(CommentRow {
            comment_id: comment.id,
            author: comment.author.clone(),
            initials: comment.initials.clone(),
            date: comment.date.clone(),
            parent_id: comment.parent_id,
        });
    }

    for revision in &doc.revisions {
        tables.revisions.push(RevisionRow {
            revision_id: revision.id.clone(),
            kind: revision.kind.clone(),
            author: revision.author.clone(),
            date: revision.date.clone(),
        });
    }

    for field in &doc.fields {
        tables.fields.push(FieldRow {
            field_id: field.id,
            instr: field.instr.clone(),
            result: field.result.clone(),
            kind: field.kind.clone(),
        });
    }

    for (idx, header) in doc.headers.iter().enumerate() {
        let header_id = (idx + 1) as u32;
        tables.headers.push(HeaderFooterRow {
            header_id,
            kind: header.kind.clone(),
            ref_type: header.ref_type.clone(),
            part: header.part.clone(),
        });
    }
    for (idx, footer) in doc.footers.iter().enumerate() {
        let footer_id = (idx + 1) as u32;
        tables.footers.push(HeaderFooterRow {
            header_id: footer_id,
            kind: footer.kind.clone(),
            ref_type: footer.ref_type.clone(),
            part: footer.part.clone(),
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

    let mut ids = IdCounters::default();

    for note in &doc.footnotes {
        flatten_blocks(
            &note.blocks,
            "footnote",
            note.id as i64,
            &mut tables,
            &mut ids,
        );
    }
    for note in &doc.endnotes {
        flatten_blocks(
            &note.blocks,
            "endnote",
            note.id as i64,
            &mut tables,
            &mut ids,
        );
    }
    for comment in &doc.comments {
        flatten_blocks(
            &comment.blocks,
            "comment",
            comment.id as i64,
            &mut tables,
            &mut ids,
        );
    }

    for (idx, header) in doc.headers.iter().enumerate() {
        let header_id = (idx + 1) as i64;
        flatten_blocks(&header.blocks, "header", header_id, &mut tables, &mut ids);
    }
    for (idx, footer) in doc.footers.iter().enumerate() {
        let footer_id = (idx + 1) as i64;
        flatten_blocks(&footer.blocks, "footer", footer_id, &mut tables, &mut ids);
    }

    flatten_blocks(&doc.blocks, "document", 0, &mut tables, &mut ids);

    tables
}

fn flatten_blocks(
    blocks: &[Block],
    owner_kind: &str,
    owner_id: i64,
    tables: &mut DocxTables,
    ids: &mut IdCounters,
) {
    for (block_index, block) in blocks.iter().enumerate() {
        match block {
            Block::Paragraph(p) => {
                let paragraph_id = ids.next_paragraph;
                ids.next_paragraph += 1;
                tables.paragraphs.push(ParagraphRow {
                    paragraph_id,
                    owner_kind: owner_kind.to_string(),
                    owner_id,
                    block_index: block_index as u32,
                    para_xml_id: p.id.clone(),
                    style_id: p.style_id.clone(),
                    style_name: p.style_name.clone(),
                    outline_level: p.outline_level,
                    num_id: p.numbering.as_ref().map(|n| n.num_id),
                    num_level: p.numbering.as_ref().map(|n| n.level),
                    toc_level: p.toc_level,
                    text: p.text.clone(),
                });
                for (run_index, run) in p.runs.iter().enumerate() {
                    let run_id = ids.next_run;
                    ids.next_run += 1;
                    tables.runs.push(RunRow {
                        run_id,
                        paragraph_id,
                        run_index: run_index as u32,
                        text: run.text.clone(),
                        style_id: run.style_id.clone(),
                        bold: run.bold,
                        italic: run.italic,
                        underline: run.underline,
                        field_id: run.field_id,
                        revision_id: run.revision.as_ref().map(|r| r.id.clone()),
                        revision_kind: run.revision.as_ref().map(|r| r.kind.clone()),
                        comment_ids: run.comment_ids.clone(),
                        footnote_ids: run
                            .note_references
                            .iter()
                            .filter(|reference| reference.kind == "footnote")
                            .map(|reference| reference.id)
                            .collect(),
                        endnote_ids: run
                            .note_references
                            .iter()
                            .filter(|reference| reference.kind == "endnote")
                            .map(|reference| reference.id)
                            .collect(),
                    });
                }
            }
            Block::Table(t) => {
                let table_id = ids.next_table;
                ids.next_table += 1;
                tables.tables.push(TableRow {
                    table_id,
                    owner_kind: owner_kind.to_string(),
                    owner_id,
                    block_index: block_index as u32,
                });
                for (row_index, row) in t.rows.iter().enumerate() {
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
                        });
                        flatten_blocks(&cell.blocks, "table_cell", cell_id as i64, tables, ids);
                    }
                }
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

fn settings_schema() -> SchemaRef {
    Arc::new(Schema::new(vec![
        Field::new("key", DataType::Utf8, false),
        Field::new("value", DataType::Utf8, false),
    ]))
}

fn page_count_schema() -> SchemaRef {
    Arc::new(Schema::new(vec![Field::new(
        "count",
        DataType::UInt32,
        false,
    )]))
}

fn toc_schema() -> SchemaRef {
    Arc::new(Schema::new(vec![
        Field::new("toc_index", DataType::UInt32, false),
        Field::new("level", DataType::UInt8, false),
        Field::new("text", DataType::Utf8, false),
        Field::new("source", DataType::Utf8, false),
        Field::new("paragraph_id", DataType::Utf8, true),
        Field::new("style_id", DataType::Utf8, true),
    ]))
}

fn styles_schema() -> SchemaRef {
    Arc::new(Schema::new(vec![
        Field::new("id", DataType::Utf8, false),
        Field::new("name", DataType::Utf8, true),
        Field::new("style_type", DataType::Utf8, true),
        Field::new("based_on", DataType::Utf8, true),
        Field::new("next", DataType::Utf8, true),
        Field::new("outline_level", DataType::UInt8, true),
        Field::new("ui_priority", DataType::UInt8, true),
        Field::new("is_default", DataType::Boolean, false),
        Field::new("is_qformat", DataType::Boolean, false),
    ]))
}

fn numbering_levels_schema() -> SchemaRef {
    Arc::new(Schema::new(vec![
        Field::new("abstract_id", DataType::UInt32, false),
        Field::new("level", DataType::UInt8, false),
        Field::new("format", DataType::Utf8, true),
        Field::new("text", DataType::Utf8, true),
        Field::new("start", DataType::UInt32, true),
    ]))
}

fn numbering_nums_schema() -> SchemaRef {
    Arc::new(Schema::new(vec![
        Field::new("num_id", DataType::UInt32, false),
        Field::new("abstract_id", DataType::UInt32, true),
    ]))
}

fn notes_schema() -> SchemaRef {
    Arc::new(Schema::new(vec![
        Field::new("note_type", DataType::Utf8, false),
        Field::new("note_id", DataType::Int32, false),
    ]))
}

fn comments_schema() -> SchemaRef {
    Arc::new(Schema::new(vec![
        Field::new("comment_id", DataType::Int32, false),
        Field::new("author", DataType::Utf8, true),
        Field::new("initials", DataType::Utf8, true),
        Field::new("date", DataType::Utf8, true),
        Field::new("parent_id", DataType::Int32, true),
    ]))
}

fn revisions_schema() -> SchemaRef {
    Arc::new(Schema::new(vec![
        Field::new("revision_id", DataType::Utf8, false),
        Field::new("kind", DataType::Utf8, false),
        Field::new("author", DataType::Utf8, true),
        Field::new("date", DataType::Utf8, true),
    ]))
}

fn fields_schema() -> SchemaRef {
    Arc::new(Schema::new(vec![
        Field::new("field_id", DataType::UInt32, false),
        Field::new("instr", DataType::Utf8, true),
        Field::new("result", DataType::Utf8, false),
        Field::new("kind", DataType::Utf8, true),
    ]))
}

fn headers_schema() -> SchemaRef {
    Arc::new(Schema::new(vec![
        Field::new("header_id", DataType::UInt32, false),
        Field::new("kind", DataType::Utf8, false),
        Field::new("ref_type", DataType::Utf8, true),
        Field::new("part", DataType::Utf8, false),
    ]))
}

fn footers_schema() -> SchemaRef {
    Arc::new(Schema::new(vec![
        Field::new("footer_id", DataType::UInt32, false),
        Field::new("kind", DataType::Utf8, false),
        Field::new("ref_type", DataType::Utf8, true),
        Field::new("part", DataType::Utf8, false),
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

fn paragraphs_schema() -> SchemaRef {
    Arc::new(Schema::new(vec![
        Field::new("paragraph_id", DataType::UInt64, false),
        Field::new("owner_kind", DataType::Utf8, false),
        Field::new("owner_id", DataType::Int64, false),
        Field::new("block_index", DataType::UInt32, false),
        Field::new("para_xml_id", DataType::Utf8, true),
        Field::new("style_id", DataType::Utf8, true),
        Field::new("style_name", DataType::Utf8, true),
        Field::new("outline_level", DataType::UInt8, true),
        Field::new("num_id", DataType::UInt32, true),
        Field::new("num_level", DataType::UInt8, true),
        Field::new("toc_level", DataType::UInt8, true),
        Field::new("text", DataType::Utf8, false),
    ]))
}

fn runs_schema() -> SchemaRef {
    Arc::new(Schema::new(vec![
        Field::new("run_id", DataType::UInt64, false),
        Field::new("paragraph_id", DataType::UInt64, false),
        Field::new("run_index", DataType::UInt32, false),
        Field::new("text", DataType::Utf8, false),
        Field::new("style_id", DataType::Utf8, true),
        Field::new("bold", DataType::Boolean, false),
        Field::new("italic", DataType::Boolean, false),
        Field::new("underline", DataType::Boolean, false),
        Field::new("field_id", DataType::UInt32, true),
        Field::new("revision_id", DataType::Utf8, true),
        Field::new("revision_kind", DataType::Utf8, true),
        Field::new(
            "comment_ids",
            DataType::List(Arc::new(Field::new("item", DataType::Int32, true))),
            false,
        ),
        Field::new(
            "footnote_ids",
            DataType::List(Arc::new(Field::new("item", DataType::Int32, true))),
            false,
        ),
        Field::new(
            "endnote_ids",
            DataType::List(Arc::new(Field::new("item", DataType::Int32, true))),
            false,
        ),
    ]))
}

fn tables_schema() -> SchemaRef {
    Arc::new(Schema::new(vec![
        Field::new("table_id", DataType::UInt64, false),
        Field::new("owner_kind", DataType::Utf8, false),
        Field::new("owner_id", DataType::Int64, false),
        Field::new("block_index", DataType::UInt32, false),
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
    ]))
}

fn emit_docx_table_batches<F>(
    tables: &DocxTables,
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
        "settings" => emit_settings_batches(&tables.settings, batch_size, schema, &mut emit),
        "page_count" => emit_page_count_batches(tables.page_count, schema, &mut emit),
        "toc" => emit_toc_batches(&tables.toc, batch_size, schema, &mut emit),
        "styles" => emit_styles_batches(&tables.styles, batch_size, schema, &mut emit),
        "numbering_levels" => {
            emit_numbering_levels_batches(&tables.numbering_levels, batch_size, schema, &mut emit)
        }
        "numbering_nums" => {
            emit_numbering_nums_batches(&tables.numbering_nums, batch_size, schema, &mut emit)
        }
        "notes" => emit_notes_batches(&tables.notes, batch_size, schema, &mut emit),
        "comments" => emit_comments_batches(&tables.comments, batch_size, schema, &mut emit),
        "revisions" => emit_revisions_batches(&tables.revisions, batch_size, schema, &mut emit),
        "fields" => emit_fields_batches(&tables.fields, batch_size, schema, &mut emit),
        "headers" => emit_headers_batches(&tables.headers, batch_size, schema, &mut emit),
        "footers" => emit_footers_batches(&tables.footers, batch_size, schema, &mut emit),
        "images" => emit_images_batches(&tables.images, batch_size, schema, &mut emit),
        "paragraphs" => emit_paragraphs_batches(&tables.paragraphs, batch_size, schema, &mut emit),
        "runs" => emit_runs_batches(&tables.runs, batch_size, schema, &mut emit),
        "tables" => emit_tables_batches(&tables.tables, batch_size, schema, &mut emit),
        "table_rows" => emit_table_rows_batches(&tables.table_rows, batch_size, schema, &mut emit),
        "table_cells" => {
            emit_table_cells_batches(&tables.table_cells, batch_size, schema, &mut emit)
        }
        _ => Err(anyhow::anyhow!("unknown docx table: {table}")),
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

fn emit_settings_batches<F>(
    rows: &[SettingRow],
    batch_size: usize,
    schema: &SchemaRef,
    emit: &mut F,
) -> Result<()>
where
    F: FnMut(RecordBatch) -> Result<()>,
{
    for chunk in rows.chunks(batch_size) {
        let mut key = StringBuilder::new();
        let mut value = StringBuilder::new();
        for row in chunk {
            key.append_value(&row.key);
            value.append_value(&row.value);
        }
        let batch = RecordBatch::try_new(
            schema.clone(),
            vec![Arc::new(key.finish()), Arc::new(value.finish())],
        )?;
        emit(batch)?;
    }
    Ok(())
}

fn emit_page_count_batches<F>(
    page_count: Option<u32>,
    schema: &SchemaRef,
    emit: &mut F,
) -> Result<()>
where
    F: FnMut(RecordBatch) -> Result<()>,
{
    if let Some(count) = page_count {
        let mut builder = UInt32Builder::new();
        builder.append_value(count);
        let batch = RecordBatch::try_new(schema.clone(), vec![Arc::new(builder.finish())])?;
        emit(batch)?;
    }
    Ok(())
}

fn emit_toc_batches<F>(
    rows: &[TocRow],
    batch_size: usize,
    schema: &SchemaRef,
    emit: &mut F,
) -> Result<()>
where
    F: FnMut(RecordBatch) -> Result<()>,
{
    for chunk in rows.chunks(batch_size) {
        let mut toc_index = UInt32Builder::new();
        let mut level = UInt8Builder::new();
        let mut text = StringBuilder::new();
        let mut source = StringBuilder::new();
        let mut paragraph_id = StringBuilder::new();
        let mut style_id = StringBuilder::new();
        for row in chunk {
            toc_index.append_value(row.toc_index);
            level.append_value(row.level);
            text.append_value(&row.text);
            source.append_value(&row.source);
            paragraph_id.append_option(row.paragraph_id.as_deref());
            style_id.append_option(row.style_id.as_deref());
        }
        let batch = RecordBatch::try_new(
            schema.clone(),
            vec![
                Arc::new(toc_index.finish()),
                Arc::new(level.finish()),
                Arc::new(text.finish()),
                Arc::new(source.finish()),
                Arc::new(paragraph_id.finish()),
                Arc::new(style_id.finish()),
            ],
        )?;
        emit(batch)?;
    }
    Ok(())
}

fn emit_styles_batches<F>(
    rows: &[StyleRow],
    batch_size: usize,
    schema: &SchemaRef,
    emit: &mut F,
) -> Result<()>
where
    F: FnMut(RecordBatch) -> Result<()>,
{
    for chunk in rows.chunks(batch_size) {
        let mut id = StringBuilder::new();
        let mut name = StringBuilder::new();
        let mut style_type = StringBuilder::new();
        let mut based_on = StringBuilder::new();
        let mut next = StringBuilder::new();
        let mut outline_level = UInt8Builder::new();
        let mut ui_priority = UInt8Builder::new();
        let mut is_default = BooleanBuilder::new();
        let mut is_qformat = BooleanBuilder::new();
        for row in chunk {
            id.append_value(&row.id);
            name.append_option(row.name.as_deref());
            style_type.append_option(row.style_type.as_deref());
            based_on.append_option(row.based_on.as_deref());
            next.append_option(row.next.as_deref());
            outline_level.append_option(row.outline_level);
            ui_priority.append_option(row.ui_priority);
            is_default.append_value(row.is_default);
            is_qformat.append_value(row.is_qformat);
        }
        let batch = RecordBatch::try_new(
            schema.clone(),
            vec![
                Arc::new(id.finish()),
                Arc::new(name.finish()),
                Arc::new(style_type.finish()),
                Arc::new(based_on.finish()),
                Arc::new(next.finish()),
                Arc::new(outline_level.finish()),
                Arc::new(ui_priority.finish()),
                Arc::new(is_default.finish()),
                Arc::new(is_qformat.finish()),
            ],
        )?;
        emit(batch)?;
    }
    Ok(())
}

fn emit_numbering_levels_batches<F>(
    rows: &[NumberingLevelRow],
    batch_size: usize,
    schema: &SchemaRef,
    emit: &mut F,
) -> Result<()>
where
    F: FnMut(RecordBatch) -> Result<()>,
{
    for chunk in rows.chunks(batch_size) {
        let mut abstract_id = UInt32Builder::new();
        let mut level = UInt8Builder::new();
        let mut format = StringBuilder::new();
        let mut text = StringBuilder::new();
        let mut start = UInt32Builder::new();
        for row in chunk {
            abstract_id.append_value(row.abstract_id);
            level.append_value(row.level);
            format.append_option(row.format.as_deref());
            text.append_option(row.text.as_deref());
            start.append_option(row.start);
        }
        let batch = RecordBatch::try_new(
            schema.clone(),
            vec![
                Arc::new(abstract_id.finish()),
                Arc::new(level.finish()),
                Arc::new(format.finish()),
                Arc::new(text.finish()),
                Arc::new(start.finish()),
            ],
        )?;
        emit(batch)?;
    }
    Ok(())
}

fn emit_numbering_nums_batches<F>(
    rows: &[NumberingNumRow],
    batch_size: usize,
    schema: &SchemaRef,
    emit: &mut F,
) -> Result<()>
where
    F: FnMut(RecordBatch) -> Result<()>,
{
    for chunk in rows.chunks(batch_size) {
        let mut num_id = UInt32Builder::new();
        let mut abstract_id = UInt32Builder::new();
        for row in chunk {
            num_id.append_value(row.num_id);
            abstract_id.append_option(row.abstract_id);
        }
        let batch = RecordBatch::try_new(
            schema.clone(),
            vec![Arc::new(num_id.finish()), Arc::new(abstract_id.finish())],
        )?;
        emit(batch)?;
    }
    Ok(())
}

fn emit_notes_batches<F>(
    rows: &[NoteRow],
    batch_size: usize,
    schema: &SchemaRef,
    emit: &mut F,
) -> Result<()>
where
    F: FnMut(RecordBatch) -> Result<()>,
{
    for chunk in rows.chunks(batch_size) {
        let mut note_type = StringBuilder::new();
        let mut note_id = Int32Builder::new();
        for row in chunk {
            note_type.append_value(&row.note_type);
            note_id.append_value(row.note_id);
        }
        let batch = RecordBatch::try_new(
            schema.clone(),
            vec![Arc::new(note_type.finish()), Arc::new(note_id.finish())],
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
        let mut comment_id = Int32Builder::new();
        let mut author = StringBuilder::new();
        let mut initials = StringBuilder::new();
        let mut date = StringBuilder::new();
        let mut parent_id = Int32Builder::new();
        for row in chunk {
            comment_id.append_value(row.comment_id);
            author.append_option(row.author.as_deref());
            initials.append_option(row.initials.as_deref());
            date.append_option(row.date.as_deref());
            parent_id.append_option(row.parent_id);
        }
        let batch = RecordBatch::try_new(
            schema.clone(),
            vec![
                Arc::new(comment_id.finish()),
                Arc::new(author.finish()),
                Arc::new(initials.finish()),
                Arc::new(date.finish()),
                Arc::new(parent_id.finish()),
            ],
        )?;
        emit(batch)?;
    }
    Ok(())
}

fn emit_revisions_batches<F>(
    rows: &[RevisionRow],
    batch_size: usize,
    schema: &SchemaRef,
    emit: &mut F,
) -> Result<()>
where
    F: FnMut(RecordBatch) -> Result<()>,
{
    for chunk in rows.chunks(batch_size) {
        let mut revision_id = StringBuilder::new();
        let mut kind = StringBuilder::new();
        let mut author = StringBuilder::new();
        let mut date = StringBuilder::new();
        for row in chunk {
            revision_id.append_value(&row.revision_id);
            kind.append_value(&row.kind);
            author.append_option(row.author.as_deref());
            date.append_option(row.date.as_deref());
        }
        let batch = RecordBatch::try_new(
            schema.clone(),
            vec![
                Arc::new(revision_id.finish()),
                Arc::new(kind.finish()),
                Arc::new(author.finish()),
                Arc::new(date.finish()),
            ],
        )?;
        emit(batch)?;
    }
    Ok(())
}

fn emit_fields_batches<F>(
    rows: &[FieldRow],
    batch_size: usize,
    schema: &SchemaRef,
    emit: &mut F,
) -> Result<()>
where
    F: FnMut(RecordBatch) -> Result<()>,
{
    for chunk in rows.chunks(batch_size) {
        let mut field_id = UInt32Builder::new();
        let mut instr = StringBuilder::new();
        let mut result = StringBuilder::new();
        let mut kind = StringBuilder::new();
        for row in chunk {
            field_id.append_value(row.field_id);
            instr.append_option(row.instr.as_deref());
            result.append_value(&row.result);
            kind.append_option(row.kind.as_deref());
        }
        let batch = RecordBatch::try_new(
            schema.clone(),
            vec![
                Arc::new(field_id.finish()),
                Arc::new(instr.finish()),
                Arc::new(result.finish()),
                Arc::new(kind.finish()),
            ],
        )?;
        emit(batch)?;
    }
    Ok(())
}

fn emit_headers_batches<F>(
    rows: &[HeaderFooterRow],
    batch_size: usize,
    schema: &SchemaRef,
    emit: &mut F,
) -> Result<()>
where
    F: FnMut(RecordBatch) -> Result<()>,
{
    for chunk in rows.chunks(batch_size) {
        let mut header_id = UInt32Builder::new();
        let mut kind = StringBuilder::new();
        let mut ref_type = StringBuilder::new();
        let mut part = StringBuilder::new();
        for row in chunk {
            header_id.append_value(row.header_id);
            kind.append_value(&row.kind);
            ref_type.append_option(row.ref_type.as_deref());
            part.append_value(&row.part);
        }
        let batch = RecordBatch::try_new(
            schema.clone(),
            vec![
                Arc::new(header_id.finish()),
                Arc::new(kind.finish()),
                Arc::new(ref_type.finish()),
                Arc::new(part.finish()),
            ],
        )?;
        emit(batch)?;
    }
    Ok(())
}

fn emit_footers_batches<F>(
    rows: &[HeaderFooterRow],
    batch_size: usize,
    schema: &SchemaRef,
    emit: &mut F,
) -> Result<()>
where
    F: FnMut(RecordBatch) -> Result<()>,
{
    for chunk in rows.chunks(batch_size) {
        let mut footer_id = UInt32Builder::new();
        let mut kind = StringBuilder::new();
        let mut ref_type = StringBuilder::new();
        let mut part = StringBuilder::new();
        for row in chunk {
            footer_id.append_value(row.header_id);
            kind.append_value(&row.kind);
            ref_type.append_option(row.ref_type.as_deref());
            part.append_value(&row.part);
        }
        let batch = RecordBatch::try_new(
            schema.clone(),
            vec![
                Arc::new(footer_id.finish()),
                Arc::new(kind.finish()),
                Arc::new(ref_type.finish()),
                Arc::new(part.finish()),
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
        let mut owner_kind = StringBuilder::new();
        let mut owner_id = Int64Builder::new();
        let mut block_index = UInt32Builder::new();
        let mut para_xml_id = StringBuilder::new();
        let mut style_id = StringBuilder::new();
        let mut style_name = StringBuilder::new();
        let mut outline_level = UInt8Builder::new();
        let mut num_id = UInt32Builder::new();
        let mut num_level = UInt8Builder::new();
        let mut toc_level = UInt8Builder::new();
        let mut text = StringBuilder::new();
        for row in chunk {
            paragraph_id.append_value(row.paragraph_id);
            owner_kind.append_value(&row.owner_kind);
            owner_id.append_value(row.owner_id);
            block_index.append_value(row.block_index);
            para_xml_id.append_option(row.para_xml_id.as_deref());
            style_id.append_option(row.style_id.as_deref());
            style_name.append_option(row.style_name.as_deref());
            outline_level.append_option(row.outline_level);
            num_id.append_option(row.num_id);
            num_level.append_option(row.num_level);
            toc_level.append_option(row.toc_level);
            text.append_value(&row.text);
        }
        let batch = RecordBatch::try_new(
            schema.clone(),
            vec![
                Arc::new(paragraph_id.finish()),
                Arc::new(owner_kind.finish()),
                Arc::new(owner_id.finish()),
                Arc::new(block_index.finish()),
                Arc::new(para_xml_id.finish()),
                Arc::new(style_id.finish()),
                Arc::new(style_name.finish()),
                Arc::new(outline_level.finish()),
                Arc::new(num_id.finish()),
                Arc::new(num_level.finish()),
                Arc::new(toc_level.finish()),
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
        let mut style_id = StringBuilder::new();
        let mut bold = BooleanBuilder::new();
        let mut italic = BooleanBuilder::new();
        let mut underline = BooleanBuilder::new();
        let mut field_id = UInt32Builder::new();
        let mut revision_id = StringBuilder::new();
        let mut revision_kind = StringBuilder::new();
        let mut comment_ids = ListBuilder::new(Int32Builder::new());
        let mut footnote_ids = ListBuilder::new(Int32Builder::new());
        let mut endnote_ids = ListBuilder::new(Int32Builder::new());
        for row in chunk {
            run_id.append_value(row.run_id);
            paragraph_id.append_value(row.paragraph_id);
            run_index.append_value(row.run_index);
            text.append_value(&row.text);
            style_id.append_option(row.style_id.as_deref());
            bold.append_value(row.bold);
            italic.append_value(row.italic);
            underline.append_value(row.underline);
            field_id.append_option(row.field_id);
            revision_id.append_option(row.revision_id.as_deref());
            revision_kind.append_option(row.revision_kind.as_deref());
            append_i32_list(&mut comment_ids, &row.comment_ids);
            append_i32_list(&mut footnote_ids, &row.footnote_ids);
            append_i32_list(&mut endnote_ids, &row.endnote_ids);
        }
        let batch = RecordBatch::try_new(
            schema.clone(),
            vec![
                Arc::new(run_id.finish()),
                Arc::new(paragraph_id.finish()),
                Arc::new(run_index.finish()),
                Arc::new(text.finish()),
                Arc::new(style_id.finish()),
                Arc::new(bold.finish()),
                Arc::new(italic.finish()),
                Arc::new(underline.finish()),
                Arc::new(field_id.finish()),
                Arc::new(revision_id.finish()),
                Arc::new(revision_kind.finish()),
                Arc::new(comment_ids.finish()),
                Arc::new(footnote_ids.finish()),
                Arc::new(endnote_ids.finish()),
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
        let mut owner_id = Int64Builder::new();
        let mut block_index = UInt32Builder::new();
        for row in chunk {
            table_id.append_value(row.table_id);
            owner_kind.append_value(&row.owner_kind);
            owner_id.append_value(row.owner_id);
            block_index.append_value(row.block_index);
        }
        let batch = RecordBatch::try_new(
            schema.clone(),
            vec![
                Arc::new(table_id.finish()),
                Arc::new(owner_kind.finish()),
                Arc::new(owner_id.finish()),
                Arc::new(block_index.finish()),
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
        for row in chunk {
            cell_id.append_value(row.cell_id);
            row_id.append_value(row.row_id);
            col_index.append_value(row.col_index);
        }
        let batch = RecordBatch::try_new(
            schema.clone(),
            vec![
                Arc::new(cell_id.finish()),
                Arc::new(row_id.finish()),
                Arc::new(col_index.finish()),
            ],
        )?;
        emit(batch)?;
    }
    Ok(())
}

fn append_i32_list(builder: &mut ListBuilder<Int32Builder>, values: &[i32]) {
    for value in values {
        builder.values().append_value(*value);
    }
    builder.append(true);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Metadata, Numbering, Paragraph, Run};
    use std::collections::HashMap;

    fn sample_doc() -> DocxDocument {
        DocxDocument {
            metadata: Metadata {
                core: HashMap::new(),
                app: HashMap::new(),
                custom: HashMap::new(),
            },
            settings: HashMap::new(),
            page_count: None,
            toc: vec![],
            styles: vec![],
            numbering: Numbering {
                abstract_nums: vec![],
                nums: vec![],
            },
            footnotes: vec![],
            endnotes: vec![],
            comments: vec![],
            revisions: vec![],
            fields: vec![],
            headers: vec![],
            footers: vec![],
            blocks: vec![Block::Paragraph(Paragraph {
                id: None,
                style_id: None,
                style_name: None,
                outline_level: None,
                numbering: None,
                toc_level: None,
                runs: vec![Run {
                    text: "hello".into(),
                    style_id: None,
                    bold: false,
                    italic: false,
                    underline: false,
                    field_id: None,
                    revision: None,
                    comment_ids: vec![],
                    note_references: vec![],
                }],
                text: "hello".into(),
            })],
            images: vec![],
        }
    }

    #[test]
    fn tables_and_schema_for_table_stay_in_lockstep() {
        // WHY: dataset writer iterates TABLES; a missing schema arm used to
        // unwrap-panic. Every TABLES entry must resolve.
        for &table in TABLES {
            assert!(
                schema_for_table(table).is_some(),
                "missing schema_for_table arm for `{table}`"
            );
        }
    }

    #[test]
    fn unknown_table_name_fails_loudly() {
        // WHY: callers must not get a silent empty stream for typos.
        let err = docx_document_to_arrow_table_batches(
            &sample_doc(),
            ArrowOptions::default(),
            "not_a_real_table",
            |_| Ok(()),
        )
        .expect_err("unknown table must error");
        assert!(err.to_string().contains("unknown docx table"));
    }

    #[test]
    fn in_memory_paragraphs_batch_has_rows() {
        // WHY: Flight/ETL should shred an already-parsed DocxDocument without
        // re-opening the zip — the in-memory path is the SOTA interchange.
        let mut rows = 0usize;
        docx_document_to_arrow_table_batches(
            &sample_doc(),
            ArrowOptions { batch_size: 1024 },
            "paragraphs",
            |batch| {
                rows += batch.num_rows();
                Ok(())
            },
        )
        .expect("paragraphs emit");
        assert!(rows >= 1, "expected at least one paragraph row, got {rows}");
    }
}

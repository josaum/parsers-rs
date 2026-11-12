use crate::{
    open_archive, open_archive_with_budget, open_optional_part_reader, open_part_reader,
    parse_relationships, parse_shared_strings_reader, parse_sheet_reader_with, parse_styles,
    parse_workbook, part_dir, read_optional_part_bytes, rels_path_for_part, resolve_part,
    CellPartsRef, CellValueRef, ParseOptions, StringTable, Styles, REL_SHARED_STRINGS, REL_STYLES,
};
use anyhow::{anyhow, Context, Result};
use arrow_array::builder::{
    BooleanBuilder, Float64Builder, GenericStringBuilder, UInt32Builder, UInt8Builder,
};
use arrow_array::{ArrayRef, RecordBatch};
use arrow_ipc::writer::StreamWriter;
use arrow_schema::{DataType, Field, Schema, SchemaRef};
use std::fs::File;
use std::io::{BufWriter, Write};
use std::sync::{mpsc, Arc, Mutex};
use std::thread;

const TYPE_EMPTY: u8 = 0;
const TYPE_BOOL: u8 = 1;
const TYPE_NUMBER: u8 = 2;
const TYPE_STRING: u8 = 3;
const TYPE_ERROR: u8 = 4;
const TYPE_DATE: u8 = 5;
const TYPE_RAW: u8 = 6;

pub use crate::RangeFilter;

/// Options controlling Arrow RecordBatch layout and parallelism.
///
/// Passed to [`parse_xlsx_to_arrow_batches`] to customize batch size, sheet
/// filtering, row/column ranges, and parallel sheet parsing. Default values
/// provide balanced throughput for most files.
#[derive(Debug, Clone)]
pub struct ArrowOptions {
    /// Number of cells to accumulate before emitting a Record Batch (default:
    /// 16384). Larger values reduce IPC overhead; smaller values reduce memory
    /// footprint during streaming.
    pub batch_size: usize,
    /// Include sheet names as a nullable string column in output batches
    /// (default: `true`). When `false`, `sheet_name` is set to NULL for all
    /// rows, reducing serialized size.
    pub include_sheet_names: bool,
    /// Restrict output to sheets whose names match this list. When `None`
    /// (default), all sheets are included. Sheet names are compared
    /// case-sensitively.
    pub sheet_filter: Option<Vec<String>>,
    /// Restrict output to cells falling within one or more row/column ranges.
    /// When `None` (default), all cells are included. Cells outside every
    /// range are silently skipped.
    pub range_filter: Option<Vec<RangeFilter>>,
    /// Number of threads to spawn for parallel sheet parsing (default: 0 means
    /// serial). When > 1 and the workbook has multiple sheets, sheets are
    /// parsed concurrently; outputs are re-ordered to maintain sequence.
    pub parallel_sheets: usize,
}

impl Default for ArrowOptions {
    fn default() -> Self {
        Self {
            batch_size: 16_384,
            include_sheet_names: true,
            sheet_filter: None,
            range_filter: None,
            parallel_sheets: 0,
        }
    }
}

/// Arrow schema for XLSX cell output: sheet_id, sheet_name, row, col,
/// value_type, and type-specific columns (bool_value, number_value,
/// string_value, error_value, date_iso, date_serial, formula).
///
/// Used by [`parse_xlsx_to_arrow_batches`]; the returned schema matches the
/// RecordBatch structure produced by that function.
pub fn schema() -> SchemaRef {
    Arc::new(Schema::new(vec![
        Field::new("sheet_id", DataType::UInt32, false),
        Field::new("sheet_name", DataType::Utf8, true),
        Field::new("row", DataType::UInt32, false),
        Field::new("col", DataType::UInt32, false),
        Field::new("value_type", DataType::UInt8, false),
        Field::new("bool_value", DataType::Boolean, true),
        Field::new("number_value", DataType::Float64, true),
        Field::new("string_value", DataType::Utf8, true),
        Field::new("error_value", DataType::Utf8, true),
        Field::new("date_iso", DataType::Utf8, true),
        Field::new("date_serial", DataType::Float64, true),
        Field::new("formula", DataType::Utf8, true),
    ]))
}

/// Parse an `.xlsx` file and encode its cell batches as an in-memory Arrow IPC
/// stream.
///
/// Use this when the caller wants the bytes — a Flight response, an in-process
/// handoff. Use [`parse_xlsx_to_arrow_ipc`] when the workbook is large enough
/// that writing straight to a file is the point.
pub fn parse_xlsx_to_arrow_ipc_bytes(
    path: impl AsRef<std::path::Path>,
    options: ParseOptions,
    arrow_options: ArrowOptions,
) -> Result<Vec<u8>> {
    let schema = schema();
    let mut ipc = Vec::new();
    {
        let mut writer = StreamWriter::try_new(&mut ipc, &schema)?;
        parse_xlsx_to_arrow_batches(path, options, arrow_options, |batch| {
            writer.write(&batch)?;
            Ok(())
        })?;
        writer.finish()?;
    }
    Ok(ipc)
}

/// Parse an `.xlsx` file and stream its cell batches to `out_path` as Arrow
/// IPC (Feather v2), returning the number of bytes written.
///
/// Batches follow [`schema()`] and are emitted as they fill, so peak memory is
/// bounded by [`ArrowOptions::batch_size`] rather than by the workbook. The
/// stream is fed through a [`BufWriter`], so a failure mid-parse leaves a
/// partial file: a caller that needs all-or-nothing output should write to a
/// temporary path and rename.
pub fn parse_xlsx_to_arrow_ipc(
    path: impl AsRef<std::path::Path>,
    out_path: impl AsRef<std::path::Path>,
    options: ParseOptions,
    arrow_options: ArrowOptions,
) -> Result<usize> {
    let schema = schema();
    let file = File::create(out_path.as_ref())
        .with_context(|| format!("create output: {}", out_path.as_ref().display()))?;
    let mut counted = CountingWriter {
        inner: BufWriter::new(file),
        written: 0,
    };
    {
        let mut writer = StreamWriter::try_new(&mut counted, &schema)?;
        parse_xlsx_to_arrow_batches(path, options, arrow_options, |batch| {
            writer.write(&batch)?;
            Ok(())
        })?;
        writer.finish()?;
    }
    counted.flush()?;
    Ok(counted.written)
}

/// Counts bytes on their way to the output file so the caller learns the size
/// of what it just wrote without a second stat call.
struct CountingWriter<W: Write> {
    inner: W,
    written: usize,
}

impl<W: Write> Write for CountingWriter<W> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let n = self.inner.write(buf)?;
        self.written += n;
        Ok(n)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.inner.flush()
    }
}

/// Stream RecordBatches to a callback without writing to disk. Allows
/// integration with Flight servers and in-memory ETL pipelines. The callback
/// is invoked for each complete batch (size controlled by
/// [`ArrowOptions::batch_size`]); returning an error halts parsing.
pub fn parse_xlsx_to_arrow_batches<F>(
    path: impl AsRef<std::path::Path>,
    options: ParseOptions,
    arrow_options: ArrowOptions,
    mut emit: F,
) -> Result<()>
where
    F: FnMut(RecordBatch) -> Result<()>,
{
    let path = path.as_ref();
    let archive = open_archive(path)?;
    let schema = schema();

    let root_rels = read_optional_part_bytes(&archive, "_rels/.rels", options.max_inflate)?;
    let workbook_path = root_rels
        .as_deref()
        .and_then(ooxml_archive::parse_root_rels)
        .unwrap_or_else(|| "xl/workbook.xml".to_string());

    let workbook_xml = crate::read_part_bytes(&archive, &workbook_path, options.max_inflate)
        .with_context(|| format!("read workbook part: {workbook_path}"))?;
    let workbook_info = parse_workbook(&workbook_xml)?;
    let workbook_dir = part_dir(&workbook_path);
    let workbook_rels_path = rels_path_for_part(&workbook_path);
    let workbook_rels_xml =
        crate::read_part_bytes(&archive, &workbook_rels_path, options.max_inflate)
            .with_context(|| format!("read workbook rels: {workbook_rels_path}"))?;
    let workbook_rels = parse_relationships(&workbook_rels_xml)?;

    let shared_strings_path = workbook_rels
        .by_type
        .get(REL_SHARED_STRINGS)
        .map(|t| resolve_part(&workbook_dir, t));
    let styles_path = workbook_rels
        .by_type
        .get(REL_STYLES)
        .map(|t| resolve_part(&workbook_dir, t));

    let shared_strings = if let Some(path) = shared_strings_path.as_deref() {
        if let Some(reader) = open_optional_part_reader(&archive, path, options.max_inflate)? {
            parse_shared_strings_reader(reader)?
        } else {
            StringTable::default()
        }
    } else {
        StringTable::default()
    };

    let styles = if options.decode_dates {
        if let Some(path) = styles_path.as_deref() {
            if let Some(xml) = read_optional_part_bytes(&archive, path, options.max_inflate)? {
                parse_styles(&xml)?
            } else {
                Styles::default()
            }
        } else {
            Styles::default()
        }
    } else {
        Styles::default()
    };

    let mut builder = ArrowBatchBuilder::new(
        arrow_options.batch_size,
        arrow_options.include_sheet_names,
        arrow_options.range_filter.clone(),
    );

    let mut tasks = Vec::new();
    for sheet in workbook_info.sheets {
        if let Some(filter) = arrow_options.sheet_filter.as_ref() {
            if !filter.iter().any(|name| name == &sheet.name) {
                continue;
            }
        }
        let target = workbook_rels
            .by_id
            .get(&sheet.rel_id)
            .ok_or_else(|| anyhow!("missing relationship for sheet {}", sheet.name))?;
        let sheet_path = resolve_part(&workbook_dir, target);
        tasks.push(SheetTask {
            sheet_id: sheet.sheet_id,
            sheet_name: sheet.name.clone(),
            sheet_path,
        });
    }

    let mut worker_count = arrow_options.parallel_sheets;
    if worker_count > 0 && tasks.len() > 1 {
        if worker_count > tasks.len() {
            worker_count = tasks.len();
        }
        run_parallel_sheets(
            path,
            archive.shared_inflate_budget(),
            tasks,
            worker_count,
            workbook_info.date_1904,
            shared_strings,
            styles,
            options,
            &arrow_options,
            &schema,
            emit,
        )?;
        return Ok(());
    }

    for task in tasks {
        parse_sheet_serial(
            &archive,
            task,
            workbook_info.date_1904,
            &shared_strings,
            &styles,
            &options,
            &arrow_options,
            &schema,
            &mut builder,
            &mut emit,
        )?;
    }

    if builder.len() > 0 {
        let batch = builder.finish(&schema)?;
        emit(batch)?;
    }

    Ok(())
}

struct SheetTask {
    sheet_id: u32,
    sheet_name: String,
    sheet_path: String,
}

// Serial sheet worker: threads the full immutable parse context (archive,
// shared strings, styles, options, schema) plus the mutable builder/emit sink.
#[allow(clippy::too_many_arguments)]
fn parse_sheet_serial<F>(
    archive: &crate::XlsxArchive,
    task: SheetTask,
    date_1904: bool,
    shared_strings: &StringTable,
    styles: &Styles,
    options: &ParseOptions,
    arrow_options: &ArrowOptions,
    schema: &SchemaRef,
    builder: &mut ArrowBatchBuilder,
    emit: &mut F,
) -> Result<()>
where
    F: FnMut(RecordBatch) -> Result<()>,
{
    let reader = open_part_reader(archive, &task.sheet_path, options.max_inflate)
        .with_context(|| format!("read sheet: {}", task.sheet_path))?;
    let xml_reader = quick_xml::Reader::from_reader(reader);
    parse_sheet_reader_with(
        xml_reader,
        shared_strings,
        styles,
        date_1904,
        options,
        arrow_options.range_filter.as_deref(),
        |cell| {
            builder.push(task.sheet_id, &task.sheet_name, cell);
            if builder.is_full() {
                let batch = builder.finish(schema)?;
                emit(batch)?;
            }
            Ok(())
        },
    )?;
    Ok(())
}

// Parallel fan-out driver: owns the shared parse context that each worker
// thread clones/Arc-shares before streaming batches back over the channel.
#[allow(clippy::too_many_arguments)]
fn run_parallel_sheets<F>(
    path: &std::path::Path,
    inflate_budget: ooxml_archive::SharedInflateBudget,
    tasks: Vec<SheetTask>,
    worker_count: usize,
    date_1904: bool,
    shared_strings: StringTable,
    styles: Styles,
    options: ParseOptions,
    arrow_options: &ArrowOptions,
    schema: &SchemaRef,
    mut emit: F,
) -> Result<()>
where
    F: FnMut(RecordBatch) -> Result<()>,
{
    let (task_tx, task_rx) = mpsc::channel::<SheetTask>();
    let (msg_tx, msg_rx) = mpsc::sync_channel::<WorkerMsg>(worker_count.saturating_mul(2).max(1));
    let path = path.to_path_buf();
    let shared_strings = Arc::new(shared_strings);
    let styles = Arc::new(styles);
    let task_rx = Arc::new(Mutex::new(task_rx));

    let mut handles = Vec::with_capacity(worker_count);
    for _ in 0..worker_count {
        let task_rx = Arc::clone(&task_rx);
        let msg_tx = msg_tx.clone();
        let path = path.clone();
        let inflate_budget = Arc::clone(&inflate_budget);
        let shared_strings = Arc::clone(&shared_strings);
        let styles = Arc::clone(&styles);
        let arrow_options = arrow_options.clone();
        let schema = schema.clone();
        handles.push(thread::spawn(move || {
            let archive = match open_archive_with_budget(&path, inflate_budget) {
                Ok(archive) => archive,
                Err(err) => {
                    let _ = msg_tx.send(WorkerMsg::Error(err));
                    return;
                }
            };
            loop {
                let task = {
                    let guard = task_rx.lock().unwrap_or_else(|e| e.into_inner());
                    guard.recv()
                };
                let task = match task {
                    Ok(task) => task,
                    Err(_) => break,
                };
                let result = parse_sheet_parallel(
                    &archive,
                    task,
                    date_1904,
                    &shared_strings,
                    &styles,
                    &options,
                    &arrow_options,
                    &schema,
                    &msg_tx,
                );
                if let Err(err) = result {
                    let _ = msg_tx.send(WorkerMsg::Error(err));
                    return;
                }
            }
        }));
    }

    for task in tasks {
        task_tx
            .send(task)
            .map_err(|_| anyhow!("worker channel closed"))?;
    }
    drop(task_tx);
    drop(msg_tx);

    let mut error: Option<anyhow::Error> = None;
    let rx = msg_rx;
    while let Ok(msg) = rx.recv() {
        match msg {
            WorkerMsg::Batch(batch) => {
                if let Err(err) = emit(batch) {
                    error = Some(err);
                    break;
                }
            }
            WorkerMsg::Error(err) => {
                error = Some(err);
                break;
            }
        }
    }
    drop(rx);

    for handle in handles {
        let _ = handle.join();
    }

    if let Some(err) = error {
        return Err(err);
    }

    Ok(())
}

// Per-thread sheet worker: same immutable parse context as the serial path,
// but emits batches over the worker channel instead of a caller callback.
#[allow(clippy::too_many_arguments)]
fn parse_sheet_parallel(
    archive: &crate::XlsxArchive,
    task: SheetTask,
    date_1904: bool,
    shared_strings: &StringTable,
    styles: &Styles,
    options: &ParseOptions,
    arrow_options: &ArrowOptions,
    schema: &SchemaRef,
    msg_tx: &mpsc::SyncSender<WorkerMsg>,
) -> Result<()> {
    let reader = open_part_reader(archive, &task.sheet_path, options.max_inflate)
        .with_context(|| format!("read sheet: {}", task.sheet_path))?;
    let xml_reader = quick_xml::Reader::from_reader(reader);
    let mut builder = ArrowBatchBuilder::new(
        arrow_options.batch_size,
        arrow_options.include_sheet_names,
        arrow_options.range_filter.clone(),
    );
    parse_sheet_reader_with(
        xml_reader,
        shared_strings,
        styles,
        date_1904,
        options,
        arrow_options.range_filter.as_deref(),
        |cell| {
            builder.push(task.sheet_id, &task.sheet_name, cell);
            if builder.is_full() {
                let batch = builder.finish(schema)?;
                if msg_tx.send(WorkerMsg::Batch(batch)).is_err() {
                    return Err(anyhow!("arrow consumer dropped"));
                }
            }
            Ok(())
        },
    )?;
    if builder.len() > 0 {
        let batch = builder.finish(schema)?;
        let _ = msg_tx.send(WorkerMsg::Batch(batch));
    }
    Ok(())
}

enum WorkerMsg {
    Batch(RecordBatch),
    Error(anyhow::Error),
}

struct ArrowBatchBuilder {
    capacity: usize,
    include_sheet_names: bool,
    range_filter: Option<Vec<RangeFilter>>,
    sheet_id: UInt32Builder,
    sheet_name: GenericStringBuilder<i32>,
    row: UInt32Builder,
    col: UInt32Builder,
    value_type: UInt8Builder,
    bool_value: BooleanBuilder,
    number_value: Float64Builder,
    string_value: GenericStringBuilder<i32>,
    error_value: GenericStringBuilder<i32>,
    date_iso: GenericStringBuilder<i32>,
    date_serial: Float64Builder,
    formula: GenericStringBuilder<i32>,
    len: usize,
    /// Text bytes appended to the string columns since the last batch.
    text_bytes: usize,
}

/// Text a batch may hold before it is closed early (64 MiB).
///
/// Every cell's text is copied into its batch, so a workbook with one long
/// shared string and many cells that refer to it would otherwise fill a batch
/// with their product: 16,384 rows of a 1 MiB string is 16 GiB, and more than
/// the 2 GiB a `Utf8` column can address. Ordinary workbooks never reach this
/// and still get `batch_size` rows per batch. Lowering it only produces more,
/// smaller batches.
const MAX_BATCH_TEXT_BYTES: usize = 64 * 1024 * 1024;

impl ArrowBatchBuilder {
    fn new(
        capacity: usize,
        include_sheet_names: bool,
        range_filter: Option<Vec<RangeFilter>>,
    ) -> Self {
        let string_bytes = capacity.saturating_mul(16);
        Self {
            capacity,
            include_sheet_names,
            range_filter,
            sheet_id: UInt32Builder::with_capacity(capacity),
            sheet_name: GenericStringBuilder::with_capacity(capacity, string_bytes),
            row: UInt32Builder::with_capacity(capacity),
            col: UInt32Builder::with_capacity(capacity),
            value_type: UInt8Builder::with_capacity(capacity),
            bool_value: BooleanBuilder::with_capacity(capacity),
            number_value: Float64Builder::with_capacity(capacity),
            string_value: GenericStringBuilder::with_capacity(capacity, string_bytes),
            error_value: GenericStringBuilder::with_capacity(capacity, string_bytes),
            date_iso: GenericStringBuilder::with_capacity(capacity, string_bytes),
            date_serial: Float64Builder::with_capacity(capacity),
            formula: GenericStringBuilder::with_capacity(capacity, string_bytes),
            len: 0,
            text_bytes: 0,
        }
    }

    fn len(&self) -> usize {
        self.len
    }

    /// Whether the batch should be emitted before another cell is added.
    fn is_full(&self) -> bool {
        self.len >= self.capacity || self.text_bytes >= MAX_BATCH_TEXT_BYTES
    }

    fn push(&mut self, sheet_id: u32, sheet_name: &str, cell: CellPartsRef<'_>) {
        if let Some(ranges) = self.range_filter.as_ref() {
            let mut matched = false;
            for range in ranges {
                if cell.row >= range.row_start
                    && cell.row <= range.row_end
                    && cell.col >= range.col_start
                    && cell.col <= range.col_end
                {
                    matched = true;
                    break;
                }
            }
            if !matched {
                return;
            }
        }
        let mut text_bytes = cell.formula.map_or(0, str::len);
        if self.include_sheet_names {
            text_bytes += sheet_name.len();
        }
        text_bytes += match &cell.value {
            CellValueRef::String { v } | CellValueRef::Raw { v } | CellValueRef::Error { v } => {
                v.len()
            }
            CellValueRef::Date { iso, .. } => iso.len(),
            CellValueRef::Empty | CellValueRef::Bool { .. } | CellValueRef::Number { .. } => 0,
        };
        self.text_bytes = self.text_bytes.saturating_add(text_bytes);

        self.sheet_id.append_value(sheet_id);
        if self.include_sheet_names {
            self.sheet_name.append_value(sheet_name);
        } else {
            self.sheet_name.append_null();
        }
        self.row.append_value(cell.row);
        self.col.append_value(cell.col);

        match cell.value {
            CellValueRef::Empty => {
                self.value_type.append_value(TYPE_EMPTY);
                self.bool_value.append_null();
                self.number_value.append_null();
                self.string_value.append_null();
                self.error_value.append_null();
                self.date_iso.append_null();
                self.date_serial.append_null();
            }
            CellValueRef::Bool { v } => {
                self.value_type.append_value(TYPE_BOOL);
                self.bool_value.append_value(v);
                self.number_value.append_null();
                self.string_value.append_null();
                self.error_value.append_null();
                self.date_iso.append_null();
                self.date_serial.append_null();
            }
            CellValueRef::Number { v } => {
                self.value_type.append_value(TYPE_NUMBER);
                self.bool_value.append_null();
                self.number_value.append_value(v);
                self.string_value.append_null();
                self.error_value.append_null();
                self.date_iso.append_null();
                self.date_serial.append_null();
            }
            CellValueRef::String { v } => {
                self.value_type.append_value(TYPE_STRING);
                self.bool_value.append_null();
                self.number_value.append_null();
                self.string_value.append_value(v);
                self.error_value.append_null();
                self.date_iso.append_null();
                self.date_serial.append_null();
            }
            CellValueRef::Raw { v } => {
                self.value_type.append_value(TYPE_RAW);
                self.bool_value.append_null();
                self.number_value.append_null();
                self.string_value.append_value(v);
                self.error_value.append_null();
                self.date_iso.append_null();
                self.date_serial.append_null();
            }
            CellValueRef::Error { v } => {
                self.value_type.append_value(TYPE_ERROR);
                self.bool_value.append_null();
                self.number_value.append_null();
                self.string_value.append_null();
                self.error_value.append_value(v);
                self.date_iso.append_null();
                self.date_serial.append_null();
            }
            CellValueRef::Date { iso, serial } => {
                self.value_type.append_value(TYPE_DATE);
                self.bool_value.append_null();
                self.number_value.append_null();
                self.string_value.append_null();
                self.error_value.append_null();
                self.date_iso.append_value(&iso);
                self.date_serial.append_value(serial);
            }
        }

        if let Some(formula) = cell.formula {
            self.formula.append_value(formula);
        } else {
            self.formula.append_null();
        }

        self.len += 1;
    }

    fn finish(&mut self, schema: &SchemaRef) -> Result<RecordBatch> {
        let arrays: Vec<ArrayRef> = vec![
            Arc::new(self.sheet_id.finish()),
            Arc::new(self.sheet_name.finish()),
            Arc::new(self.row.finish()),
            Arc::new(self.col.finish()),
            Arc::new(self.value_type.finish()),
            Arc::new(self.bool_value.finish()),
            Arc::new(self.number_value.finish()),
            Arc::new(self.string_value.finish()),
            Arc::new(self.error_value.finish()),
            Arc::new(self.date_iso.finish()),
            Arc::new(self.date_serial.finish()),
            Arc::new(self.formula.finish()),
        ];
        let batch = RecordBatch::try_new(schema.clone(), arrays)
            .map_err(|err| anyhow!("build record batch: {err}"))?;
        *self = ArrowBatchBuilder::new(
            self.capacity,
            self.include_sheet_names,
            self.range_filter.clone(),
        );
        Ok(batch)
    }
}

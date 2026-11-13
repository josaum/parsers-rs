//! The `.xlsx` Flight server. The server itself is
//! `flight_ipc_config::table_server`, shared with the other parsers' servers;
//! this file says what is particular to `.xlsx`: one `cells` table, its own
//! command line, and its own query keys (`sheet`, `range`, `raw`, `dates`,
//! `formulas`, `sheet_names`, `batch`, `threads`).
use std::path::{Path, PathBuf};

use arrow_array::RecordBatch;
use arrow_schema::SchemaRef;
use flight_ipc_config::table_server::{TableFlightService, TableParser};
use flight_ipc_config::{
    capability_json, parse_writer_policies_for_process, url_decode, FlightRequestAuthVerifier,
    ParserRoute,
};
use tonic::Status;
use xlsx_fast_core::arrow::{self, ArrowOptions, RangeFilter};
use xlsx_fast_core::{find_first_digit, ParseOptions};

#[cfg(test)]
#[path = "../../flight-ipc-config/test-support/mod.rs"]
mod flight_auth_test_support;

#[cfg(test)]
use arrow_flight::flight_service_server::FlightService;
#[cfg(all(test, unix))]
use arrow_flight::FlightData;
#[cfg(test)]
use arrow_flight::{Action, Empty, FlightDescriptor};
#[cfg(test)]
use arrow_ipc::writer::IpcWriteOptions;
#[cfg(test)]
use flight_ipc_config::{do_put_unsupported, CAPABILITY_ACTION};
#[cfg(test)]
use tonic::Request;

/// What `.xlsx` contributes to the shared server.
struct Xlsx;

impl TableParser for Xlsx {
    type ParseOptions = ParseOptions;
    type ArrowOptions = ArrowOptions;

    const ROUTE: ParserRoute = ParserRoute::Xlsx;
    const SERVER_NAME: &'static str = "xlsx-fast-flight";
    const DEFAULT_BIND: &'static str = "0.0.0.0:50051";
    const DEFAULT_TABLE: &'static str = "cells";
    const MISSING_DEFAULT_SCHEMA: &'static str = "missing schema for default xlsx table";
    const EXTENSIONS: &'static [&'static str] = &["xlsx", "xlsm", "xltx", "xltm"];
    // Every workbook has the same schema, so GetSchema has never looked at
    // the file it is asked about.
    const GET_SCHEMA_READS_DESCRIPTOR: bool = false;

    fn parse_options(max_inflate: usize) -> ParseOptions {
        ParseOptions {
            decode_dates: true,
            include_formulas: false,
            raw_values: false,
            max_inflate,
        }
    }

    fn arrow_options(batch_size: usize) -> ArrowOptions {
        ArrowOptions {
            batch_size,
            include_sheet_names: true,
            sheet_filter: None,
            range_filter: None,
            parallel_sheets: 0,
        }
    }

    fn set_max_inflate(options: &mut ParseOptions, max_inflate: usize) {
        options.max_inflate = max_inflate;
    }

    fn set_batch_size(options: &mut ArrowOptions, batch_size: usize) {
        options.batch_size = batch_size;
    }

    fn list_tables() -> &'static [&'static str] {
        &[Self::DEFAULT_TABLE]
    }

    fn schema_for_table(table: &str) -> Option<SchemaRef> {
        (table == Self::DEFAULT_TABLE).then(arrow::schema)
    }

    fn parse_table_batches<F>(
        path: &Path,
        parse_options: ParseOptions,
        arrow_options: ArrowOptions,
        _table: &str,
        emit: F,
    ) -> anyhow::Result<()>
    where
        F: FnMut(RecordBatch) -> anyhow::Result<()>,
    {
        arrow::parse_xlsx_to_arrow_batches(path, parse_options, arrow_options, emit)
    }

    /// The workbook keys, not the shared ones: `table` and `max_inflate` are
    /// ignored here as they always were.
    fn apply_query(
        query: &str,
        _table: &mut Option<String>,
        parse_options: &mut ParseOptions,
        arrow_options: &mut ArrowOptions,
    ) -> Result<(), Status> {
        let query = parse_query(query)?;
        apply_query_overrides(&query, parse_options, arrow_options)?;
        if let Some(filter) = query.sheet_filter {
            arrow_options.sheet_filter = Some(filter);
        }
        if let Some(range) = query.range {
            arrow_options.range_filter = Some(range);
        }
        Ok(())
    }
}

type XlsxFlightService = TableFlightService<Xlsx>;

/// The name the tests below have always called the listing walk by.
#[cfg(test)]
fn collect_xlsx_paths(root: &Path, limit: usize) -> Result<Vec<PathBuf>, Status> {
    flight_ipc_config::table_server::collect_paths::<Xlsx>(root, limit)
}

#[derive(Default, Debug, Clone)]
struct FlightQuery {
    sheet_filter: Option<Vec<String>>,
    range: Option<Vec<RangeFilter>>,
    raw_values: Option<bool>,
    decode_dates: Option<bool>,
    include_formulas: Option<bool>,
    include_sheet_names: Option<bool>,
    batch_size: Option<usize>,
    parallel_sheets: Option<usize>,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let mut bind = "0.0.0.0:50051".to_string();
    let mut root = std::env::current_dir()?;
    let mut batch_size = 16_384usize;
    let mut decode_dates = true;
    let mut include_formulas = false;
    let mut include_sheet_names = true;
    let mut parallel_sheets = 0usize;
    let mut raw_values = false;

    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--bind" => {
                if let Some(v) = args.next() {
                    bind = v;
                }
            }
            "--root" => {
                if let Some(v) = args.next() {
                    root = PathBuf::from(v);
                }
            }
            "--batch-size" => {
                if let Some(v) = args.next() {
                    if let Ok(parsed) = v.parse::<usize>() {
                        batch_size = parsed;
                    }
                }
            }
            "--threads" => {
                if let Some(v) = args.next() {
                    if v == "auto" {
                        parallel_sheets = std::thread::available_parallelism()
                            .map(|n| n.get())
                            .unwrap_or(0);
                    } else if let Ok(parsed) = v.parse::<usize>() {
                        parallel_sheets = parsed;
                    }
                }
            }
            "--no-dates" => decode_dates = false,
            "--include-formulas" => include_formulas = true,
            "--no-sheet-names" => include_sheet_names = false,
            "--raw-values" => raw_values = true,
            _ => {}
        }
    }

    let auth_verifier = FlightRequestAuthVerifier::from_env_required()?;
    let authorized_write = parse_writer_policies_for_process(
        std::env::var("PARSERS_FLIGHT_IPC_COMPRESSION")
            .ok()
            .as_deref(),
        std::env::var("PARSERS_FLIGHT_IPC_COMPRESSION_OVERRIDES")
            .ok()
            .as_deref(),
        ParserRoute::Xlsx,
    )?
    .authorized_write()?;
    let capability_body = serde_json::to_vec(&capability_json(
        ParserRoute::Xlsx,
        Some(authorized_write.codec()),
    )?)?;
    let write_options = authorized_write.into_ipc_write_options();

    let service = XlsxFlightService {
        root,
        parse_options: ParseOptions {
            decode_dates,
            include_formulas,
            raw_values,
            max_inflate: 128 * 1024 * 1024,
        },
        arrow_options: ArrowOptions {
            batch_size,
            include_sheet_names,
            sheet_filter: None,
            range_filter: None,
            parallel_sheets,
        },
        write_options,
        capability_body,
        auth_verifier,
    };

    let addr = bind.parse()?;
    println!("[xlsx-fast-flight] listening on {addr}");

    tonic::transport::Server::builder()
        .add_service(arrow_flight::flight_service_server::FlightServiceServer::new(service))
        .serve(addr)
        .await?;

    Ok(())
}

fn parse_query(query: &str) -> Result<FlightQuery, Status> {
    let mut out = FlightQuery::default();
    for pair in query.split('&') {
        if pair.is_empty() {
            continue;
        }
        let mut iter = pair.splitn(2, '=');
        let key = iter.next().unwrap_or("").trim();
        let value = iter.next().unwrap_or("");
        if key.is_empty() {
            continue;
        }
        match key {
            "sheet" | "sheets" => {
                let decoded = url_decode(value);
                let mut list: Vec<String> = decoded
                    .split(',')
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty())
                    .collect();
                if !list.is_empty() {
                    if let Some(existing) = out.sheet_filter.as_mut() {
                        existing.append(&mut list);
                    } else {
                        out.sheet_filter = Some(list);
                    }
                }
            }
            "range" => {
                let decoded = url_decode(value);
                if !decoded.trim().is_empty() {
                    let mut list = parse_range_list(decoded.trim())?;
                    if list.is_empty() {
                        continue;
                    }
                    if let Some(existing) = out.range.as_mut() {
                        existing.append(&mut list);
                    } else {
                        out.range = Some(list);
                    }
                }
            }
            "raw" | "raw_values" => {
                out.raw_values = Some(parse_bool(value)?);
            }
            "dates" | "decode_dates" => {
                out.decode_dates = Some(parse_bool(value)?);
            }
            "formulas" | "include_formulas" => {
                out.include_formulas = Some(parse_bool(value)?);
            }
            "sheet_names" | "include_sheet_names" => {
                out.include_sheet_names = Some(parse_bool(value)?);
            }
            "batch" | "batch_size" => {
                let decoded = url_decode(value);
                let parsed = decoded
                    .parse::<usize>()
                    .map_err(|_| Status::invalid_argument("batch must be a positive integer"))?;
                if parsed == 0 {
                    return Err(Status::invalid_argument("batch must be > 0"));
                }
                out.batch_size = Some(parsed);
            }
            "threads" | "parallel_sheets" => {
                let decoded = url_decode(value);
                let parsed = decoded.parse::<usize>().map_err(|_| {
                    Status::invalid_argument("threads must be a non-negative integer")
                })?;
                out.parallel_sheets = Some(parsed);
            }
            _ => {}
        }
    }
    Ok(out)
}

fn apply_query_overrides(
    query: &FlightQuery,
    parse_options: &mut ParseOptions,
    arrow_options: &mut ArrowOptions,
) -> Result<(), Status> {
    if let Some(raw) = query.raw_values {
        parse_options.raw_values = raw;
    }
    if let Some(decode_dates) = query.decode_dates {
        parse_options.decode_dates = decode_dates;
    }
    if let Some(include_formulas) = query.include_formulas {
        parse_options.include_formulas = include_formulas;
    }
    if let Some(include_sheet_names) = query.include_sheet_names {
        arrow_options.include_sheet_names = include_sheet_names;
    }
    if let Some(batch_size) = query.batch_size {
        arrow_options.batch_size = batch_size;
    }
    if let Some(parallel_sheets) = query.parallel_sheets {
        arrow_options.parallel_sheets = parallel_sheets;
    }
    if parse_options.raw_values {
        parse_options.decode_dates = false;
    }
    Ok(())
}

fn parse_bool(input: &str) -> Result<bool, Status> {
    let value = url_decode(input);
    let value = value.trim().to_ascii_lowercase();
    match value.as_str() {
        "1" | "true" | "yes" | "y" | "on" => Ok(true),
        "0" | "false" | "no" | "n" | "off" => Ok(false),
        _ => Err(Status::invalid_argument("invalid boolean value")),
    }
}

fn parse_range_list(input: &str) -> Result<Vec<RangeFilter>, Status> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return Err(Status::invalid_argument("range cannot be empty"));
    }
    let mut out = Vec::new();
    for part in trimmed.split([',', ';']) {
        let part = part.trim();
        if part.is_empty() {
            continue;
        }
        out.push(parse_range_part(part)?);
    }
    if out.is_empty() {
        Err(Status::invalid_argument("range cannot be empty"))
    } else {
        Ok(out)
    }
}

fn parse_range_part(input: &str) -> Result<RangeFilter, Status> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return Err(Status::invalid_argument("range cannot be empty"));
    }

    let mut parts = trimmed.splitn(2, ':');
    let start = parts.next().unwrap_or("").trim();
    let end = parts.next().map(|s| s.trim());

    let start_ref = if start.is_empty() {
        None
    } else {
        Some(parse_ref_kind(start).ok_or_else(|| Status::invalid_argument("invalid range start"))?)
    };
    let end_ref = if let Some(end) = end {
        if end.is_empty() {
            None
        } else {
            Some(parse_ref_kind(end).ok_or_else(|| Status::invalid_argument("invalid range end"))?)
        }
    } else {
        None
    };

    let kind = match (start_ref, end_ref) {
        (Some(RefKind::Cell(_, _)), Some(RefKind::Cell(_, _))) => RefKindKind::Cell,
        (Some(RefKind::Row(_)), Some(RefKind::Row(_))) => RefKindKind::Row,
        (Some(RefKind::Col(_)), Some(RefKind::Col(_))) => RefKindKind::Col,
        (Some(RefKind::Cell(_, _)), None) | (None, Some(RefKind::Cell(_, _))) => RefKindKind::Cell,
        (Some(RefKind::Row(_)), None) | (None, Some(RefKind::Row(_))) => RefKindKind::Row,
        (Some(RefKind::Col(_)), None) | (None, Some(RefKind::Col(_))) => RefKindKind::Col,
        (None, None) => {
            return Err(Status::invalid_argument("range cannot be empty"));
        }
        _ => {
            return Err(Status::invalid_argument(
                "range must use cell:cell, row:row, or col:col",
            ));
        }
    };

    match kind {
        RefKindKind::Cell => {
            let (sr, sc) = match start_ref {
                Some(RefKind::Cell(r, c)) => (r, c),
                None => (1, 1),
                _ => unreachable!(),
            };
            let (er, ec) = match end_ref {
                Some(RefKind::Cell(r, c)) => (r, c),
                None => (u32::MAX, u32::MAX),
                _ => unreachable!(),
            };
            Ok(make_range(sr, er, sc, ec))
        }
        RefKindKind::Row => {
            let sr = match start_ref {
                Some(RefKind::Row(r)) => r,
                None => 1,
                _ => unreachable!(),
            };
            let er = match end_ref {
                Some(RefKind::Row(r)) => r,
                None => u32::MAX,
                _ => unreachable!(),
            };
            Ok(make_range(sr, er, 1, u32::MAX))
        }
        RefKindKind::Col => {
            let sc = match start_ref {
                Some(RefKind::Col(c)) => c,
                None => 1,
                _ => unreachable!(),
            };
            let ec = match end_ref {
                Some(RefKind::Col(c)) => c,
                None => u32::MAX,
                _ => unreachable!(),
            };
            Ok(make_range(1, u32::MAX, sc, ec))
        }
    }
}

fn make_range(sr: u32, er: u32, sc: u32, ec: u32) -> RangeFilter {
    let (row_start, row_end) = if sr <= er { (sr, er) } else { (er, sr) };
    let (col_start, col_end) = if sc <= ec { (sc, ec) } else { (ec, sc) };
    RangeFilter {
        row_start,
        row_end,
        col_start,
        col_end,
    }
}

#[derive(Clone, Copy)]
enum RefKind {
    Cell(u32, u32),
    Row(u32),
    Col(u32),
}

#[derive(Clone, Copy)]
enum RefKindKind {
    Cell,
    Row,
    Col,
}

fn parse_ref_kind(input: &str) -> Option<RefKind> {
    let bytes = input.as_bytes();
    if bytes.is_empty() {
        return None;
    }
    let digit_idx = find_first_digit(bytes);
    let mut col: u32 = 0;
    let mut row: u32 = 0;
    let mut has_col = false;
    let mut has_row = false;

    let col_end = digit_idx.unwrap_or(bytes.len());
    for &b in &bytes[..col_end] {
        if b == b'$' {
            continue;
        }
        if b.is_ascii_alphabetic() {
            has_col = true;
            let upper = b & 0xDF;
            col = col
                .checked_mul(26)?
                .checked_add(u32::from(upper - b'A' + 1))?;
        } else {
            break;
        }
    }

    if let Some(start) = digit_idx {
        for &b in &bytes[start..] {
            if b == b'$' {
                continue;
            }
            if b.is_ascii_digit() {
                has_row = true;
                row = row.checked_mul(10)?.checked_add(u32::from(b - b'0'))?;
            } else {
                break;
            }
        }
    }

    match (has_col, has_row) {
        (true, true) if row > 0 && col > 0 => Some(RefKind::Cell(row, col)),
        (true, false) if col > 0 => Some(RefKind::Col(col)),
        (false, true) if row > 0 => Some(RefKind::Row(row)),
        _ => None,
    }
}

#[cfg(test)]
mod range_reference_tests {
    use super::*;

    /// Inputs for the digit scan: every length up to 80 bytes, a digit at
    /// every position, surrounded by letters, `$`, the ASCII neighbours of
    /// the digits, and bytes that differ from a digit only in the high bit.
    fn digit_scan_inputs() -> Vec<Vec<u8>> {
        let fillers = [b'A', b'$', b'/', b':', 0x00, 0x7F, 0xB0, 0xB9, 0xFF];
        let mut inputs = Vec::new();
        for len in 0..=80usize {
            for &filler in &fillers {
                inputs.push(vec![filler; len]);
                for position in 0..len {
                    for digit in *b"059" {
                        let mut bytes = vec![filler; len];
                        bytes[position] = digit;
                        inputs.push(bytes.clone());
                        bytes[len - 1] = b'7';
                        inputs.push(bytes);
                    }
                }
            }
        }
        inputs
    }

    /// WHY: this server used to carry its own copy of the vectorized digit
    /// scan. It now calls the one in xlsx-fast-core, which must keep giving
    /// the answer a plain byte-by-byte scan gives.
    #[test]
    fn the_shared_digit_scan_agrees_with_a_byte_by_byte_scan() {
        let inputs = digit_scan_inputs();
        assert!(inputs.len() > 150_000, "{} inputs", inputs.len());
        for input in &inputs {
            assert_eq!(
                find_first_digit(input),
                input.iter().position(u8::is_ascii_digit),
                "input {input:?}"
            );
        }
    }

    fn range(input: &str) -> Option<(u32, u32, u32, u32)> {
        parse_range_part(input).ok().map(|range| {
            (
                range.row_start,
                range.row_end,
                range.col_start,
                range.col_end,
            )
        })
    }

    /// WHY: range queries are parsed around the digit scan. These results
    /// were recorded with the server's own copy of the scan and must not move
    /// when it is replaced by the shared one. The long references are 16
    /// bytes or more, which is where the vectorized path starts.
    #[test]
    fn range_queries_parse_the_same_references() {
        let all = u32::MAX;
        assert_eq!(range("A1:C3"), Some((1, 3, 1, 3)));
        assert_eq!(range("c3:a1"), Some((1, 3, 1, 3)));
        assert_eq!(range("$B$2:$D$10"), Some((2, 10, 2, 4)));
        assert_eq!(range("B2"), Some((2, all, 2, all)));
        assert_eq!(range(":B2"), Some((1, 2, 1, 2)));
        assert_eq!(range("2:5"), Some((2, 5, 1, all)));
        assert_eq!(range("B:D"), Some((1, all, 2, 4)));
        assert_eq!(range("AA10:XFD1048576"), Some((10, 1_048_576, 27, 16_384)));
        assert_eq!(range("$$$$$$$$$$$$$$$$AB$12"), Some((12, all, 28, all)));
        assert_eq!(
            range("$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$C7:$$$$$$$$$$$$$$$$$$$$E9"),
            Some((7, 9, 3, 5))
        );
        assert_eq!(
            range("$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$ZZ"),
            Some((1, all, 702, all))
        );
        assert_eq!(
            range("0000000000000000000000000000000000042"),
            Some((42, all, 1, all))
        );
        assert_eq!(range("A1:5"), None);
    }

    /// WHY: a range comes straight from the client's query. A column or row
    /// too large for the arithmetic used to panic in a debug build and wrap
    /// to an unrelated range in a release build; it is an invalid argument.
    #[test]
    fn a_reference_too_large_to_compute_is_an_invalid_argument() {
        for input in [
            "AAAAAAAAAAAAAAAA1",
            "ZZZZZZZ",
            "A99999999999",
            "99999999999",
            "A1:ZZZZZZZZ9",
            "1:4294967296",
        ] {
            let err = parse_range_part(input).expect_err(input);
            assert_eq!(err.code(), tonic::Code::InvalidArgument, "{input}");
        }
        // The largest values that still fit are accepted as before.
        assert_eq!(
            range("4294967295"),
            Some((4_294_967_295, u32::MAX, 1, u32::MAX))
        );
    }

    #[test]
    fn range_queries_reject_malformed_references() {
        assert_eq!(range("A:5"), None);
        assert_eq!(range("A0"), None);
        assert_eq!(range("!!"), None);
        assert_eq!(range(""), None);
        assert_eq!(range(":"), None);
    }
}

#[cfg(test)]
mod symlink_listing_tests {
    use super::*;
    #[cfg(unix)]
    use std::os::unix::fs::symlink as symlink_file;
    #[cfg(windows)]
    use std::os::windows::fs::symlink_file;

    #[test]
    fn collect_xlsx_paths_skips_symlinks() {
        // WHY: listing must not follow symlinks that could escape the Flight
        // root; canonicalize on DoGet is not enough if ListFlights advertises
        // the linked path.
        let root = tempfile::tempdir().expect("tempdir");
        let outside = tempfile::tempdir().expect("outside");
        let real = outside.path().join("escape.xlsx");
        std::fs::write(&real, b"PK").expect("write");
        let link = root.path().join("escape.xlsx");
        if let Err(err) = symlink_file(&real, &link) {
            #[cfg(windows)]
            if err.raw_os_error() == Some(1314) {
                return;
            }
            panic!("symlink: {err}");
        }
        let inside = root.path().join("ok.xlsx");
        std::fs::write(&inside, b"PK").expect("write inside");

        let found = collect_xlsx_paths(root.path(), 100).expect("collect");
        assert_eq!(found.len(), 1);
        assert_eq!(found[0], inside);
    }
}

#[cfg(test)]
mod read_only_flight_contract_tests {
    use super::*;
    use crate::flight_auth_test_support::{required_auth_verifier, with_valid_bearer};

    fn capability_test_service() -> XlsxFlightService {
        XlsxFlightService {
            root: PathBuf::new(),
            parse_options: ParseOptions {
                decode_dates: true,
                include_formulas: false,
                raw_values: false,
                max_inflate: 128 * 1024 * 1024,
            },
            arrow_options: ArrowOptions {
                batch_size: 64,
                include_sheet_names: true,
                sheet_filter: None,
                range_filter: None,
                parallel_sheets: 0,
            },
            write_options: IpcWriteOptions::default(),
            capability_body: b"{}".to_vec(),
            auth_verifier: required_auth_verifier(),
        }
    }

    #[test]
    fn do_put_fails_loud_as_read_only() {
        // WHY: there is no write-back protocol; DoPut must fail loud with a
        // stable Unimplemented message so clients do not treat silence as OK.
        let status = do_put_unsupported();
        assert_eq!(status.code(), tonic::Code::Unimplemented);
        assert!(status.message().contains("read-only"));
        assert!(status.message().contains("do_put"));
    }

    #[tokio::test]
    async fn capability_action_and_listing_reject_missing_authorization() {
        let service = capability_test_service();
        let Err(action) = FlightService::do_action(
            &service,
            Request::new(Action {
                r#type: CAPABILITY_ACTION.to_owned(),
                body: Default::default(),
            }),
        )
        .await
        else {
            panic!("missing capability auth must fail");
        };
        let Err(list) = FlightService::list_actions(&service, Request::new(Empty {})).await else {
            panic!("missing listing auth must fail");
        };
        assert_eq!(action.code(), tonic::Code::Unauthenticated);
        assert_eq!(list.code(), tonic::Code::Unauthenticated);
    }

    #[tokio::test]
    async fn retrieval_rejects_missing_authorization_before_request_parsing() {
        let service = capability_test_service();
        let result = FlightService::get_schema(
            &service,
            Request::new(FlightDescriptor::new_path(vec![
                "__missing_auth_probe__.xlsx".to_owned(),
            ])),
        )
        .await;
        let Err(error) = result else {
            panic!("unauthenticated retrieval must fail");
        };
        assert_eq!(error.code(), tonic::Code::Unauthenticated);
    }

    #[tokio::test]
    async fn valid_bearer_reaches_retrieval_dispatch() {
        let service = capability_test_service();
        let request = with_valid_bearer(Request::new(FlightDescriptor::new_path(vec![
            "__missing_auth_probe__.xlsx".to_owned(),
        ])));
        assert!(FlightService::get_schema(&service, request).await.is_ok());
    }
}

#[cfg(all(test, unix))]
#[path = "../../flight-ipc-config/test-support/wire.rs"]
mod flight_wire_test_support;

#[cfg(all(test, unix))]
mod wire_parity_tests {
    //! What a Flight client sees from this server, pinned as text in
    //! `tests/wire_transcript.txt`.
    //!
    //! The transcript was captured from the server while it still carried its
    //! own copy of the Flight skeleton, so a change to any answer in it is a
    //! change in behaviour, not in structure. Unix only: the probes include a
    //! symbolic link and a backslash in a file name.
    use super::*;
    use crate::flight_auth_test_support::required_auth_verifier;
    use crate::flight_wire_test_support::{stored_zip, Wire};

    const EXPECTED: &str = include_str!("../tests/wire_transcript.txt");

    /// The one-sheet workbook `xlsx-fast-core`'s own tests build
    /// (`tests/parser_integration.rs`), as a stored archive.
    fn fixture() -> Vec<u8> {
        stored_zip(&[
            (
                "_rels/.rels",
                r#"<?xml version="1.0" encoding="UTF-8"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
  <Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="xl/workbook.xml"/>
</Relationships>"#,
            ),
            (
                "xl/workbook.xml",
                r#"<?xml version="1.0" encoding="UTF-8"?>
<workbook xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships">
  <workbookPr date1904="false"/>
  <sheets>
    <sheet name="Sheet1" sheetId="1" r:id="rId1"/>
  </sheets>
</workbook>"#,
            ),
            (
                "xl/_rels/workbook.xml.rels",
                r#"<?xml version="1.0" encoding="UTF-8"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
  <Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/worksheet" Target="worksheets/sheet1.xml"/>
  <Relationship Id="rId2" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/sharedStrings" Target="sharedStrings.xml"/>
</Relationships>"#,
            ),
            (
                "xl/sharedStrings.xml",
                r#"<?xml version="1.0" encoding="UTF-8"?>
<sst xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main" count="1" uniqueCount="1">
  <si><t>Hello XLSX</t></si>
</sst>"#,
            ),
            (
                "xl/worksheets/sheet1.xml",
                r#"<?xml version="1.0" encoding="UTF-8"?>
<worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main">
  <sheetData>
    <row r="1">
      <c r="A1" t="s"><v>0</v></c>
      <c r="B1"><v>42</v></c>
      <c r="C1"><f>SUM(B1,B1)</f><v>84</v></c>
    </row>
    <row r="2">
      <c r="A2"><v>7</v></c>
    </row>
  </sheetData>
</worksheet>"#,
            ),
        ])
    }

    fn service(root: &Path) -> XlsxFlightService {
        XlsxFlightService {
            root: root.to_path_buf(),
            parse_options: ParseOptions {
                decode_dates: true,
                include_formulas: false,
                raw_values: false,
                max_inflate: 128 * 1024 * 1024,
            },
            arrow_options: ArrowOptions {
                batch_size: 64,
                include_sheet_names: true,
                sheet_filter: None,
                range_filter: None,
                parallel_sheets: 0,
            },
            write_options: IpcWriteOptions::default(),
            capability_body: b"{}".to_vec(),
            auth_verifier: required_auth_verifier(),
        }
    }

    #[tokio::test]
    async fn a_file_that_cannot_be_opened_is_named_as_the_caller_named_it() {
        use std::os::unix::fs::PermissionsExt;

        let base = tempfile::tempdir().expect("tempdir");
        let root = base.path().join("root");
        std::fs::create_dir_all(&root).expect("directories");
        let locked = root.join("locked.xlsx");
        std::fs::write(&locked, fixture()).expect("fixture");
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000))
            .expect("permissions");
        if std::fs::File::open(&locked).is_ok() {
            // A superuser opens the file anyway, so there is no error to read.
            return;
        }

        let mut wire = Wire::start(service(&root), base.path()).await;
        wire.do_get(b"locked.xlsx", true).await;

        assert!(wire.seen.contains("locked.xlsx"), "{}", wire.seen);
        assert_eq!(
            wire.answers_naming_server_paths,
            Vec::<String>::new(),
            "an answer named a directory on the server"
        );
    }

    #[tokio::test]
    async fn a_path_outside_the_root_is_answered_like_a_missing_one() {
        let base = tempfile::tempdir().expect("tempdir");
        let root = base.path().join("root");
        std::fs::create_dir_all(&root).expect("directories");
        let mut wire = Wire::start(service(&root), base.path()).await;
        wire.assert_outside_paths_look_missing(base.path(), "xlsx")
            .await;
    }

    #[tokio::test]
    async fn every_rpc_answers_as_it_did_before_the_shared_server() {
        let base = tempfile::tempdir().expect("tempdir");
        let root = base.path().join("root");
        std::fs::create_dir_all(root.join("sub")).expect("directories");
        let fixture = fixture();
        std::fs::write(root.join("book.xlsx"), &fixture).expect("fixture");
        std::fs::write(root.join("sub").join("Upper.XLSX"), &fixture).expect("nested");
        std::fs::write(root.join("notes.txt"), b"not a workbook").expect("other file");
        std::fs::write(base.path().join("outside.xlsx"), &fixture).expect("outside");
        std::os::unix::fs::symlink(base.path().join("outside.xlsx"), root.join("link.xlsx"))
            .expect("symlink");
        let outside = base.path().join("outside.xlsx");
        let outside = outside.to_string_lossy();

        let mut wire = Wire::start(service(&root), base.path()).await;

        wire.handshake(false).await;
        wire.handshake(true).await;
        wire.list_flights(false).await;
        wire.list_flights(true).await;

        wire.get_flight_info("book.xlsx", false).await;
        for path in [
            "book.xlsx",
            "book.xlsx?sheet=Sheet1&range=A1:B2",
            "book.xlsx?table=x&max_inflate=0",
            "book.xlsx?batch=0",
            "book.xlsx?batch=x",
            "book.xlsx?raw=maybe",
            "book.xlsx?range=A:5",
            "book.xlsx?threads=x",
            "sub/Upper.XLSX",
            "sub\\Upper.XLSX",
            "missing.xlsx",
            "../outside.xlsx",
            "link.xlsx",
            outside.as_ref(),
        ] {
            wire.get_flight_info(path, true).await;
        }
        wire.get_flight_info_parts(&["sub", "Upper.XLSX?batch=2"], true)
            .await;
        wire.get_flight_info_parts(&["sub", "..", "book.xlsx"], true)
            .await;
        wire.get_flight_info_cmd(b"book.xlsx?batch=2", true).await;
        wire.get_flight_info_cmd(b"book.xlsx?batch=0", true).await;
        wire.get_flight_info_cmd(b"\xff", true).await;
        wire.get_flight_info_cmd(b"", true).await;

        wire.poll_flight_info("book.xlsx", false).await;
        wire.poll_flight_info("book.xlsx", true).await;

        wire.get_schema("book.xlsx", false).await;
        wire.get_schema("book.xlsx", true).await;
        wire.get_schema("missing.xlsx?batch=0", true).await;

        wire.do_get(b"book.xlsx", false).await;
        for ticket in [
            "book.xlsx",
            "book.xlsx?batch=1",
            "book.xlsx?sheet=Nope",
            "book.xlsx?sheets=Sheet1,Nope&range=B1:C1",
            "book.xlsx?raw=1&formulas=yes&sheet_names=off&dates=0&threads=2",
            "book.xlsx?table=x&max_inflate=0",
            "book.xlsx?batch=0",
            "book.xlsx?range=A:5",
            "book.xlsx?",
            "sub/Upper.XLSX",
            "sub\\Upper.XLSX",
            "notes.txt",
            "missing.xlsx",
            "../outside.xlsx",
            "sub/../book.xlsx",
            "link.xlsx",
            outside.as_ref(),
            "",
        ] {
            wire.do_get(ticket.as_bytes(), true).await;
        }
        wire.do_get(b"\xff", true).await;
        wire.do_get(b"sub", true).await;

        wire.do_put(false).await;
        wire.do_put(true).await;

        wire.do_exchange_path("book.xlsx", false).await;
        wire.do_exchange_path("book.xlsx", true).await;
        wire.do_exchange_path("book.xlsx?batch=0", true).await;
        wire.do_exchange_path("missing.xlsx", true).await;
        wire.do_exchange_metadata("book.xlsx?batch=1", true).await;
        wire.do_exchange_metadata("../outside.xlsx", true).await;
        wire.do_exchange("an empty message", vec![FlightData::default()], true)
            .await;
        wire.do_exchange("no message", Vec::new(), true).await;

        wire.do_action(CAPABILITY_ACTION, "", true, false).await;
        wire.do_action(CAPABILITY_ACTION, "", true, true).await;
        wire.do_action("unknown", "", true, true).await;
        wire.list_actions(false).await;
        wire.list_actions(true).await;
        wire.path_probes(&root, base.path(), "xlsx").await;

        assert_eq!(wire.seen, EXPECTED, "\n<<<WIRE\n{}WIRE>>>", wire.seen);
        assert_eq!(
            wire.answers_naming_server_paths,
            Vec::<String>::new(),
            "an answer named a directory on the server"
        );
    }
}

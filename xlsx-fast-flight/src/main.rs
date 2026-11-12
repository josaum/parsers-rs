use anyhow::anyhow;
use arrow_flight::encode::FlightDataEncoderBuilder;
use arrow_flight::error::FlightError;
use arrow_flight::flight_service_server::{FlightService, FlightServiceServer};
use arrow_flight::{
    Action, ActionType, Criteria, Empty, FlightData, FlightDescriptor, FlightEndpoint, FlightInfo,
    HandshakeRequest, HandshakeResponse, PollInfo, PutResult, SchemaResult, Ticket,
};
use arrow_ipc::writer::IpcWriteOptions;
use flight_ipc_config::{
    capability_json, do_put_unsupported, parse_writer_policies_for_process, resolve_path,
    split_query, url_decode, FlightIpcDirection, FlightIpcObservationGuard,
    FlightRequestAuthVerifier, FlightRpcAction, ParserRoute, CAPABILITY_ACTION,
};
use futures::stream::BoxStream;
use futures::StreamExt;
use std::collections::VecDeque;
use std::fs;
use std::path::{Path, PathBuf};
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use tonic::{Request, Response, Status, Streaming};

#[cfg(test)]
#[path = "../../flight-ipc-config/test-support/mod.rs"]
mod flight_auth_test_support;
use xlsx_fast_core::arrow::{self, ArrowOptions, RangeFilter};
use xlsx_fast_core::{find_first_digit, ParseOptions};

#[derive(Clone)]
struct XlsxFlightService {
    root: PathBuf,
    parse_options: ParseOptions,
    arrow_options: ArrowOptions,
    write_options: IpcWriteOptions,
    /// Serialized once at startup; DoAction must not reserialize on every call.
    capability_body: Vec<u8>,
    auth_verifier: FlightRequestAuthVerifier,
}

impl XlsxFlightService {
    fn authorize<T>(&self, request: &Request<T>) -> Result<(), Status> {
        self.auth_verifier.verify_metadata(request.metadata())
    }
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

const MAX_LIST_FLIGHTS: usize = 1024;

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
        .add_service(FlightServiceServer::new(service))
        .serve(addr)
        .await?;

    Ok(())
}

#[tonic::async_trait]
impl FlightService for XlsxFlightService {
    type HandshakeStream = ReceiverStream<Result<HandshakeResponse, Status>>;
    type ListFlightsStream = ReceiverStream<Result<FlightInfo, Status>>;
    type DoGetStream = BoxStream<'static, Result<FlightData, Status>>;
    type DoPutStream = ReceiverStream<Result<PutResult, Status>>;
    type DoExchangeStream = BoxStream<'static, Result<FlightData, Status>>;
    type DoActionStream = ReceiverStream<Result<arrow_flight::Result, Status>>;
    type ListActionsStream = ReceiverStream<Result<ActionType, Status>>;

    async fn handshake(
        &self,
        _request: Request<Streaming<HandshakeRequest>>,
    ) -> Result<Response<Self::HandshakeStream>, Status> {
        let (_tx, rx) = mpsc::channel(1);
        Ok(Response::new(ReceiverStream::new(rx)))
    }

    async fn list_flights(
        &self,
        request: Request<Criteria>,
    ) -> Result<Response<Self::ListFlightsStream>, Status> {
        self.authorize(&request)?;
        let root = self.root.clone();
        let schema = arrow::schema();
        let (tx, rx) = mpsc::channel(4);

        std::thread::spawn(move || {
            let root_canon = match root.canonicalize() {
                Ok(path) => path,
                Err(_) => {
                    let _ = tx.blocking_send(Err(Status::internal("failed to resolve root")));
                    return;
                }
            };
            let paths = match collect_xlsx_paths(&root_canon, MAX_LIST_FLIGHTS) {
                Ok(list) => list,
                Err(err) => {
                    let _ = tx.blocking_send(Err(err));
                    return;
                }
            };

            for path in paths {
                let rel = match path.strip_prefix(&root_canon) {
                    Ok(rel) => rel,
                    Err(_) => continue,
                };
                let components = path_components(rel);
                let rel_str = components.join("/");
                let ticket = Ticket::new(rel_str);
                let descriptor = FlightDescriptor::new_path(components);
                let endpoint = FlightEndpoint::new().with_ticket(ticket);

                let mut info = match FlightInfo::new().try_with_schema(schema.as_ref()) {
                    Ok(info) => info,
                    Err(err) => {
                        let _ = tx.blocking_send(Err(to_status(err)));
                        return;
                    }
                };
                info = info.with_endpoint(endpoint).with_descriptor(descriptor);

                if let Ok(meta) = fs::metadata(&path) {
                    info = info.with_total_bytes(meta.len() as i64);
                }

                if tx.blocking_send(Ok(info)).is_err() {
                    return;
                }
            }
        });

        Ok(Response::new(ReceiverStream::new(rx)))
    }

    async fn get_flight_info(
        &self,
        request: Request<FlightDescriptor>,
    ) -> Result<Response<FlightInfo>, Status> {
        self.authorize(&request)?;
        let descriptor = request.into_inner();
        let (path, query) = descriptor_to_request(&self.root, &descriptor)?;
        ensure_query_supported(&query)?;
        let ticket = Ticket::new(path_to_ticket(&self.root, &path));

        let schema = arrow::schema();
        let endpoint = FlightEndpoint::new().with_ticket(ticket);
        let mut info = FlightInfo::new()
            .try_with_schema(schema.as_ref())
            .map_err(to_status)?
            .with_endpoint(endpoint)
            .with_descriptor(descriptor);
        if let Ok(meta) = fs::metadata(&path) {
            info = info.with_total_bytes(meta.len() as i64);
        }

        Ok(Response::new(info))
    }

    async fn poll_flight_info(
        &self,
        _request: Request<FlightDescriptor>,
    ) -> Result<Response<PollInfo>, Status> {
        Err(Status::unimplemented("poll_flight_info"))
    }

    async fn get_schema(
        &self,
        request: Request<FlightDescriptor>,
    ) -> Result<Response<SchemaResult>, Status> {
        self.authorize(&request)?;
        let schema = arrow::schema();
        let options = IpcWriteOptions::default();
        let schema_ipc = arrow_flight::SchemaAsIpc::new(schema.as_ref(), &options);
        let result = SchemaResult::try_from(schema_ipc).map_err(to_status)?;
        Ok(Response::new(result))
    }

    async fn do_get(
        &self,
        request: Request<Ticket>,
    ) -> Result<Response<Self::DoGetStream>, Status> {
        self.authorize(&request)?;
        let observation = FlightIpcObservationGuard::new(
            ParserRoute::Xlsx,
            FlightIpcDirection::Read,
            FlightRpcAction::DoGet,
        );
        let ticket = request.into_inner();
        let (path, query) = ticket_to_request(&self.root, &ticket)?;
        ensure_query_supported(&query)?;
        let mut parse_options = self.parse_options;
        let mut arrow_options = self.arrow_options.clone();
        apply_query_overrides(&query, &mut parse_options, &mut arrow_options)?;
        if let Some(filter) = query.sheet_filter {
            arrow_options.sheet_filter = Some(filter);
        }
        if let Some(range) = query.range {
            arrow_options.range_filter = Some(range);
        }
        let schema = arrow::schema();

        let (tx, rx) =
            mpsc::channel::<std::result::Result<arrow_array::RecordBatch, FlightError>>(2);

        std::thread::spawn(move || {
            let result =
                arrow::parse_xlsx_to_arrow_batches(&path, parse_options, arrow_options, |batch| {
                    tx.blocking_send(Ok(batch))
                        .map_err(|_| anyhow!("flight client dropped"))?;
                    Ok(())
                });
            if let Err(err) = result {
                let io_err = std::io::Error::other(err);
                let _ = tx.blocking_send(Err(FlightError::from_external_error(Box::new(io_err))));
            }
        });

        let batch_stream = ReceiverStream::new(rx);
        let write_options = self.write_options.clone();
        let flight_stream = FlightDataEncoderBuilder::new()
            .with_schema(schema)
            .with_options(write_options)
            .build(batch_stream);

        let output = flight_ipc_config::observe_flight_stream(
            flight_stream.map(|res| res.map_err(to_status)),
            observation,
        );
        Ok(Response::new(output))
    }

    async fn do_put(
        &self,
        _request: Request<Streaming<FlightData>>,
    ) -> Result<Response<Self::DoPutStream>, Status> {
        Err(do_put_unsupported())
    }

    async fn do_exchange(
        &self,
        request: Request<Streaming<FlightData>>,
    ) -> Result<Response<Self::DoExchangeStream>, Status> {
        self.authorize(&request)?;
        let observation = FlightIpcObservationGuard::new(
            ParserRoute::Xlsx,
            FlightIpcDirection::Read,
            FlightRpcAction::DoExchange,
        );
        let mut inbound = request.into_inner();
        let first = inbound
            .message()
            .await
            .map_err(|_| Status::invalid_argument("invalid exchange stream"))?
            .ok_or_else(|| Status::invalid_argument("exchange missing initial message"))?;

        tokio::spawn(async move {
            let mut inbound = inbound;
            while let Ok(Some(_)) = inbound.message().await {}
        });

        let (path, query) = if let Some(descriptor) = first.flight_descriptor {
            descriptor_to_request(&self.root, &descriptor)?
        } else if !first.app_metadata.is_empty() {
            let ticket = Ticket::new(first.app_metadata);
            ticket_to_request(&self.root, &ticket)?
        } else {
            return Err(Status::invalid_argument(
                "exchange requires descriptor or app_metadata path",
            ));
        };

        let mut parse_options = self.parse_options;
        let mut arrow_options = self.arrow_options.clone();
        apply_query_overrides(&query, &mut parse_options, &mut arrow_options)?;
        if let Some(filter) = query.sheet_filter {
            arrow_options.sheet_filter = Some(filter);
        }
        if let Some(range) = query.range {
            arrow_options.range_filter = Some(range);
        }
        let schema = arrow::schema();

        let (tx, rx) =
            mpsc::channel::<std::result::Result<arrow_array::RecordBatch, FlightError>>(2);
        std::thread::spawn(move || {
            let result =
                arrow::parse_xlsx_to_arrow_batches(&path, parse_options, arrow_options, |batch| {
                    tx.blocking_send(Ok(batch))
                        .map_err(|_| anyhow!("flight client dropped"))?;
                    Ok(())
                });
            if let Err(err) = result {
                let io_err = std::io::Error::other(err);
                let _ = tx.blocking_send(Err(FlightError::from_external_error(Box::new(io_err))));
            }
        });

        let batch_stream = ReceiverStream::new(rx);
        let write_options = self.write_options.clone();
        let flight_stream = FlightDataEncoderBuilder::new()
            .with_schema(schema)
            .with_options(write_options)
            .build(batch_stream);

        let output = flight_ipc_config::observe_flight_stream(
            flight_stream.map(|res| res.map_err(to_status)),
            observation,
        );
        Ok(Response::new(output))
    }

    async fn do_action(
        &self,
        request: Request<Action>,
    ) -> Result<Response<Self::DoActionStream>, Status> {
        self.authorize(&request)?;
        let action = request.into_inner();
        if action.r#type != CAPABILITY_ACTION {
            return Err(Status::unimplemented("do_action"));
        }
        let (tx, rx) = mpsc::channel(1);
        tx.send(Ok(arrow_flight::Result {
            body: self.capability_body.clone().into(),
        }))
        .await
        .map_err(|_| Status::internal("capability response failed"))?;
        Ok(Response::new(ReceiverStream::new(rx)))
    }

    async fn list_actions(
        &self,
        request: Request<Empty>,
    ) -> Result<Response<Self::ListActionsStream>, Status> {
        self.authorize(&request)?;
        let (tx, rx) = mpsc::channel(1);
        tx.send(Ok(ActionType {
            r#type: CAPABILITY_ACTION.to_owned(),
            description: "cached codec capabilities".to_owned(),
        }))
        .await
        .map_err(|_| Status::internal("capability listing failed"))?;
        Ok(Response::new(ReceiverStream::new(rx)))
    }
}

fn descriptor_to_request(
    root: &Path,
    descriptor: &FlightDescriptor,
) -> Result<(PathBuf, FlightQuery), Status> {
    if !descriptor.path.is_empty() {
        let mut parts = descriptor.path.clone();
        let mut query = FlightQuery::default();
        if let Some(last) = parts.last_mut() {
            let last_value = last.clone();
            if let Some((base, q)) = split_query(&last_value) {
                *last = base.to_string();
                query = parse_query(q)?;
            }
        }
        let path = parts.into_iter().fold(PathBuf::new(), |mut buf, part| {
            buf.push(part);
            buf
        });
        resolve_path(root, &path).map(|path| (path, query))
    } else if !descriptor.cmd.is_empty() {
        let cmd = std::str::from_utf8(&descriptor.cmd)
            .map_err(|_| Status::invalid_argument("descriptor cmd must be utf-8 path"))?;
        let (path_str, query) = split_query(cmd).unwrap_or((cmd, ""));
        let query = if query.is_empty() {
            FlightQuery::default()
        } else {
            parse_query(query)?
        };
        resolve_path(root, Path::new(path_str)).map(|path| (path, query))
    } else {
        Err(Status::invalid_argument("missing flight descriptor path"))
    }
}

fn ticket_to_request(root: &Path, ticket: &Ticket) -> Result<(PathBuf, FlightQuery), Status> {
    let path = std::str::from_utf8(&ticket.ticket)
        .map_err(|_| Status::invalid_argument("ticket must be utf-8 path"))?;
    let (path_str, query_str) = split_query(path).unwrap_or((path, ""));
    let query = if query_str.is_empty() {
        FlightQuery::default()
    } else {
        parse_query(query_str)?
    };
    resolve_path(root, Path::new(path_str)).map(|path| (path, query))
}

fn path_to_ticket(root: &Path, path: &Path) -> String {
    if let Ok(root_canon) = root.canonicalize() {
        if let Ok(rel) = path.strip_prefix(&root_canon) {
            return path_components(rel).join("/");
        }
    }
    path.to_string_lossy().to_string()
}

fn path_components(path: &Path) -> Vec<String> {
    path.components()
        .map(|comp| comp.as_os_str().to_string_lossy().to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

fn collect_xlsx_paths(root: &Path, limit: usize) -> Result<Vec<PathBuf>, Status> {
    let mut out = Vec::new();
    let mut queue = VecDeque::new();
    queue.push_back(root.to_path_buf());

    while let Some(dir) = queue.pop_front() {
        let entries = match fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(_) => continue,
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let file_type = match entry.file_type() {
                Ok(ft) => ft,
                Err(_) => continue,
            };
            if file_type.is_symlink() {
                continue;
            }
            if file_type.is_dir() {
                queue.push_back(path);
                continue;
            }
            if file_type.is_file() && is_xlsx_file(&path) {
                out.push(path);
                if out.len() >= limit {
                    return Ok(out);
                }
            }
        }
    }

    Ok(out)
}

fn is_xlsx_file(path: &Path) -> bool {
    let ext = match path.extension().and_then(|s| s.to_str()) {
        Some(ext) => ext.to_ascii_lowercase(),
        None => return false,
    };
    matches!(ext.as_str(), "xlsx" | "xlsm" | "xltx" | "xltm")
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

fn ensure_query_supported(_query: &FlightQuery) -> Result<(), Status> {
    Ok(())
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
            col = col * 26 + (upper - b'A' + 1) as u32;
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
                row = row * 10 + (b - b'0') as u32;
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

fn to_status(err: impl std::fmt::Display) -> Status {
    Status::internal(err.to_string())
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

use anyhow::anyhow;
use arrow_flight::encode::FlightDataEncoderBuilder;
use arrow_flight::error::FlightError;
use arrow_flight::flight_service_server::{FlightService, FlightServiceServer};
use arrow_flight::{
    Action, ActionType, Criteria, Empty, FlightData, FlightDescriptor, FlightEndpoint, FlightInfo,
    HandshakeRequest, HandshakeResponse, PollInfo, PutResult, SchemaResult, Ticket,
};
use arrow_ipc::writer::IpcWriteOptions;
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

use docx_fast_core::arrow::{self, ArrowOptions};
use docx_fast_core::ParseOptions;
use flight_ipc_config::{
    capability_json, descriptor_to_request, do_put_unsupported, parse_writer_policies_for_process,
    ticket_to_request, FlightIpcDirection, FlightIpcObservationGuard, FlightQuery,
    FlightRequestAuthVerifier, FlightRpcAction, ParserRoute, CAPABILITY_ACTION,
};

#[derive(Clone)]
struct DocxFlightService {
    root: PathBuf,
    parse_options: ParseOptions,
    arrow_options: ArrowOptions,
    write_options: IpcWriteOptions,
    /// Serialized once at startup; DoAction must not reserialize on every call.
    capability_body: Vec<u8>,
    auth_verifier: FlightRequestAuthVerifier,
}

impl DocxFlightService {
    fn authorize<T>(&self, request: &Request<T>) -> Result<(), Status> {
        self.auth_verifier.verify_metadata(request.metadata())
    }
}

const DEFAULT_TABLE: &str = "paragraphs";
const MAX_LIST_FLIGHTS: usize = 1024;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let mut bind = "0.0.0.0:50052".to_string();
    let mut root = std::env::current_dir()?;
    let mut batch_size = 16_384usize;
    let mut max_inflate = 128 * 1024 * 1024usize;

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
            "--max-inflate" => {
                if let Some(v) = args.next() {
                    if let Ok(parsed) = v.parse::<usize>() {
                        max_inflate = parsed;
                    }
                }
            }
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
        ParserRoute::Docx,
    )?
    .authorized_write()?;
    let capability_body = serde_json::to_vec(&capability_json(
        ParserRoute::Docx,
        Some(authorized_write.codec()),
    )?)?;
    let write_options = authorized_write.into_ipc_write_options();

    let service = DocxFlightService {
        root,
        parse_options: ParseOptions {
            max_inflate,
            ..ParseOptions::default()
        },
        arrow_options: ArrowOptions { batch_size },
        write_options,
        capability_body,
        auth_verifier,
    };

    let addr = bind.parse()?;
    println!("[docx-fast-flight] listening on {addr}");

    tonic::transport::Server::builder()
        .add_service(FlightServiceServer::new(service))
        .serve(addr)
        .await?;

    Ok(())
}

#[tonic::async_trait]
impl FlightService for DocxFlightService {
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
        let schema = arrow::schema_for_table(DEFAULT_TABLE)
            .ok_or_else(|| Status::internal("missing schema for default docx table"))?;
        let (tx, rx) = mpsc::channel(4);

        std::thread::spawn(move || {
            let root_canon = match root.canonicalize() {
                Ok(path) => path,
                Err(_) => {
                    let _ = tx.blocking_send(Err(Status::internal("failed to resolve root")));
                    return;
                }
            };
            let paths = match collect_docx_paths(&root_canon, MAX_LIST_FLIGHTS) {
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
        let (_path, query) = descriptor_to_request(&self.root, &descriptor)?;
        ensure_query_supported(&query)?;
        let table = query.table.as_deref().unwrap_or(DEFAULT_TABLE);
        let schema = arrow::schema_for_table(table)
            .ok_or_else(|| Status::invalid_argument("unknown table"))?;
        let ticket = Ticket::new(path_to_ticket(&self.root, &_path));
        let endpoint = FlightEndpoint::new().with_ticket(ticket);
        let mut info = FlightInfo::new()
            .try_with_schema(schema.as_ref())
            .map_err(to_status)?
            .with_endpoint(endpoint)
            .with_descriptor(descriptor);
        if let Ok(meta) = fs::metadata(&_path) {
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
        let descriptor = request.into_inner();
        let (_path, query) = descriptor_to_request(&self.root, &descriptor)?;
        ensure_query_supported(&query)?;
        let table = query.table.as_deref().unwrap_or(DEFAULT_TABLE);
        let schema = arrow::schema_for_table(table)
            .ok_or_else(|| Status::invalid_argument("unknown table"))?;
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
            ParserRoute::Docx,
            FlightIpcDirection::Read,
            FlightRpcAction::DoGet,
        );
        let ticket = request.into_inner();
        let (path, query) = ticket_to_request(&self.root, &ticket)?;
        ensure_query_supported(&query)?;
        let table = query.table.as_deref().unwrap_or(DEFAULT_TABLE).to_string();

        let mut parse_options = self.parse_options;
        let mut arrow_options = self.arrow_options.clone();
        apply_query_overrides(&query, &mut parse_options, &mut arrow_options)?;

        let schema = arrow::schema_for_table(&table)
            .ok_or_else(|| Status::invalid_argument("unknown table"))?;

        let (tx, rx) =
            mpsc::channel::<std::result::Result<arrow_array::RecordBatch, FlightError>>(2);

        std::thread::spawn(move || {
            let result = arrow::parse_docx_to_arrow_table_batches(
                &path,
                parse_options,
                arrow_options,
                &table,
                |batch| {
                    tx.blocking_send(Ok(batch))
                        .map_err(|_| anyhow!("flight client dropped"))?;
                    Ok(())
                },
            );
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
            ParserRoute::Docx,
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

        ensure_query_supported(&query)?;
        let table = query.table.as_deref().unwrap_or(DEFAULT_TABLE).to_string();

        let mut parse_options = self.parse_options;
        let mut arrow_options = self.arrow_options.clone();
        apply_query_overrides(&query, &mut parse_options, &mut arrow_options)?;

        let schema = arrow::schema_for_table(&table)
            .ok_or_else(|| Status::invalid_argument("unknown table"))?;

        let (tx, rx) =
            mpsc::channel::<std::result::Result<arrow_array::RecordBatch, FlightError>>(2);
        std::thread::spawn(move || {
            let result = arrow::parse_docx_to_arrow_table_batches(
                &path,
                parse_options,
                arrow_options,
                &table,
                |batch| {
                    tx.blocking_send(Ok(batch))
                        .map_err(|_| anyhow!("flight client dropped"))?;
                    Ok(())
                },
            );
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

fn collect_docx_paths(root: &Path, limit: usize) -> Result<Vec<PathBuf>, Status> {
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
            if file_type.is_file() && is_docx_file(&path) {
                out.push(path);
                if out.len() >= limit {
                    return Ok(out);
                }
            }
        }
    }

    Ok(out)
}

fn is_docx_file(path: &Path) -> bool {
    let ext = match path.extension().and_then(|s| s.to_str()) {
        Some(ext) => ext.to_ascii_lowercase(),
        None => return false,
    };
    matches!(ext.as_str(), "docx" | "docm" | "dotx" | "dotm")
}

fn ensure_query_supported(query: &FlightQuery) -> Result<(), Status> {
    if let Some(table) = query.table.as_deref() {
        if !arrow::list_tables().contains(&table) {
            return Err(Status::invalid_argument("unknown table"));
        }
    }
    Ok(())
}

fn apply_query_overrides(
    query: &FlightQuery,
    parse_options: &mut ParseOptions,
    arrow_options: &mut ArrowOptions,
) -> Result<(), Status> {
    if let Some(batch_size) = query.batch_size {
        arrow_options.batch_size = batch_size;
    }
    if let Some(max_inflate) = query.max_inflate {
        parse_options.max_inflate = max_inflate;
    }
    Ok(())
}

fn to_status(err: impl std::fmt::Display) -> Status {
    Status::internal(err.to_string())
}

#[cfg(test)]
mod symlink_listing_tests {
    use super::*;
    #[cfg(unix)]
    use std::os::unix::fs::symlink as symlink_file;
    #[cfg(windows)]
    use std::os::windows::fs::symlink_file;

    #[test]
    fn collect_docx_paths_skips_symlinks() {
        // WHY: listing must not follow symlinks that could escape the Flight
        // root; canonicalize on DoGet is not enough if ListFlights advertises
        // the linked path.
        let root = tempfile::tempdir().expect("tempdir");
        let outside = tempfile::tempdir().expect("outside");
        let real = outside.path().join("escape.docx");
        std::fs::write(&real, b"PK").expect("write");
        let link = root.path().join("escape.docx");
        if let Err(err) = symlink_file(&real, &link) {
            #[cfg(windows)]
            if err.raw_os_error() == Some(1314) {
                return;
            }
            panic!("symlink: {err}");
        }
        let inside = root.path().join("ok.docx");
        std::fs::write(&inside, b"PK").expect("write inside");

        let found = collect_docx_paths(root.path(), 100).expect("collect");
        assert_eq!(found.len(), 1);
        assert_eq!(found[0], inside);
    }
}

#[cfg(test)]
mod read_only_flight_contract_tests {
    use super::*;
    use crate::flight_auth_test_support::{required_auth_verifier, with_valid_bearer};

    fn capability_test_service() -> DocxFlightService {
        DocxFlightService {
            root: PathBuf::new(),
            parse_options: ParseOptions {
                max_inflate: 128 * 1024 * 1024,
                ..ParseOptions::default()
            },
            arrow_options: ArrowOptions { batch_size: 64 },
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
                "__missing_auth_probe__.docx".to_owned(),
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
            "__missing_auth_probe__.docx".to_owned(),
        ])));
        let Err(error) = FlightService::get_schema(&service, request).await else {
            panic!("missing test file must reach path validation");
        };
        assert_eq!(error.code(), tonic::Code::NotFound);
    }
}

#[cfg(test)]
mod wire_parity_tests {
    //! What a Flight client sees from this server, pinned as text.
    //!
    //! The expectations were captured from the server as it stood before the
    //! docx and pptx servers were moved onto one shared implementation, so a
    //! change to any answer below is a change in behaviour, not in structure.
    //! The fixture is the minimal document `docx-fast-core`'s own tests build
    //! (`tests/parser_integration.rs`), written as a stored archive.
    use super::*;
    use crate::flight_auth_test_support::{required_auth_verifier, with_valid_bearer};
    use arrow_flight::flight_service_client::FlightServiceClient;
    use std::fmt::Write as _;
    use tonic::transport::server::TcpIncoming;
    use tonic::transport::Channel;

    const FIXTURE: &[u8] = include_bytes!("../tests/fixtures/minimal.docx");

    const EXPECTED: &str = r#"ListFlights without a bearer
    Unauthenticated: missing authorization metadata
ListFlights
    2 flights
    path=minimal.docx tickets=["minimal.docx"] total_bytes=1204 total_records=-1 schema_len=768 schema_fnv=ae21ca40a19657b6
    path=sub/Upper.DOCX tickets=["sub/Upper.DOCX"] total_bytes=1204 total_records=-1 schema_len=768 schema_fnv=ae21ca40a19657b6
GetFlightInfo without a bearer
    Unauthenticated: missing authorization metadata
GetFlightInfo minimal.docx
    path=minimal.docx tickets=["minimal.docx"] total_bytes=1204 total_records=-1 schema_len=768 schema_fnv=ae21ca40a19657b6
GetFlightInfo minimal.docx?table=metadata
    path=minimal.docx?table=metadata tickets=["minimal.docx"] total_bytes=1204 total_records=-1 schema_len=256 schema_fnv=8e01a2d3ba4b5fd8
GetFlightInfo minimal.docx?table=no_such_table
    InvalidArgument: unknown table
GetFlightInfo sub/Upper.DOCX
    path=sub/Upper.DOCX tickets=["sub/Upper.DOCX"] total_bytes=1204 total_records=-1 schema_len=768 schema_fnv=ae21ca40a19657b6
GetFlightInfo missing.docx
    NotFound: file not found
GetFlightInfo ../minimal.docx
    NotFound: file not found
DoGet without a bearer
    Unauthenticated: missing authorization metadata
DoGet minimal.docx
    messages=2 bytes=2812 fnv=ea48e40955674597 batches=1 rows=3 columns=["paragraph_id", "owner_kind", "owner_id", "block_index", "para_xml_id", "style_id", "style_name", "outline_level", "num_id", "num_level", "toc_level", "text"]
DoGet minimal.docx?table=metadata
    messages=1 bytes=196 fnv=7ddb901585f5ee7c batches=0 rows=0 columns=[]
DoGet minimal.docx?table=paragraphs&batch=1
    messages=4 bytes=6988 fnv=e45cf4cd242e9f09 batches=3 rows=3 columns=["paragraph_id", "owner_kind", "owner_id", "block_index", "para_xml_id", "style_id", "style_name", "outline_level", "num_id", "num_level", "toc_level", "text"]
DoGet minimal.docx?table=no_such_table
    InvalidArgument: unknown table
DoGet sub/Upper.DOCX
    messages=2 bytes=2812 fnv=ea48e40955674597 batches=1 rows=3 columns=["paragraph_id", "owner_kind", "owner_id", "block_index", "para_xml_id", "style_id", "style_name", "outline_level", "num_id", "num_level", "toc_level", "text"]
DoGet notes.txt
    Internal: External error: open zip archive
DoGet missing.docx
    NotFound: file not found
DoGet ../minimal.docx
    NotFound: file not found
DoPut without a bearer
    Unimplemented: do_put is not supported; Flight servers are read-only parse egress (no write-back)
DoPut
    Unimplemented: do_put is not supported; Flight servers are read-only parse egress (no write-back)
"#;

    fn fnv1a(parts: &[&[u8]]) -> u64 {
        let mut hash = 0xcbf2_9ce4_8422_2325_u64;
        for byte in parts.iter().flat_map(|part| part.iter()) {
            hash ^= u64::from(*byte);
            hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
        }
        hash
    }

    fn status_line(status: &Status) -> String {
        format!("{:?}: {}", status.code(), status.message())
    }

    fn info_line(info: &FlightInfo) -> String {
        let path = info
            .flight_descriptor
            .as_ref()
            .map(|descriptor| descriptor.path.join("/"))
            .unwrap_or_default();
        let tickets: Vec<String> = info
            .endpoint
            .iter()
            .filter_map(|endpoint| endpoint.ticket.as_ref())
            .map(|ticket| String::from_utf8_lossy(&ticket.ticket).into_owned())
            .collect();
        format!(
            "path={path} tickets={tickets:?} total_bytes={} total_records={} schema_len={} schema_fnv={:016x}",
            info.total_bytes,
            info.total_records,
            info.schema.len(),
            fnv1a(&[&info.schema]),
        )
    }

    async fn client_for(root: &Path) -> FlightServiceClient<Channel> {
        let service = DocxFlightService {
            root: root.to_path_buf(),
            parse_options: ParseOptions {
                max_inflate: 128 * 1024 * 1024,
                ..ParseOptions::default()
            },
            arrow_options: ArrowOptions { batch_size: 64 },
            write_options: IpcWriteOptions::default(),
            capability_body: b"{}".to_vec(),
            auth_verifier: required_auth_verifier(),
        };
        let incoming =
            TcpIncoming::bind("127.0.0.1:0".parse().expect("loopback address")).expect("bind");
        let addr = incoming.local_addr().expect("local address");
        tokio::spawn(
            tonic::transport::Server::builder()
                .add_service(FlightServiceServer::new(service))
                .serve_with_incoming(incoming),
        );
        let channel = tonic::transport::Endpoint::from_shared(format!("http://{addr}"))
            .expect("endpoint")
            .connect()
            .await
            .expect("connect");
        FlightServiceClient::new(channel)
    }

    async fn list_flights(
        client: &mut FlightServiceClient<Channel>,
        request: Request<Criteria>,
    ) -> String {
        let mut stream = match client.list_flights(request).await {
            Ok(response) => response.into_inner(),
            Err(status) => return status_line(&status),
        };
        // Directory order belongs to the operating system, not to the server.
        let mut lines = Vec::new();
        loop {
            match stream.message().await {
                Ok(Some(info)) => lines.push(info_line(&info)),
                Ok(None) => break,
                Err(status) => lines.push(status_line(&status)),
            }
        }
        lines.sort();
        format!("{} flights\n    {}", lines.len(), lines.join("\n    "))
    }

    async fn get_flight_info(
        client: &mut FlightServiceClient<Channel>,
        request: Request<FlightDescriptor>,
    ) -> String {
        match client.get_flight_info(request).await {
            Ok(response) => info_line(&response.into_inner()),
            Err(status) => status_line(&status),
        }
    }

    async fn do_get(client: &mut FlightServiceClient<Channel>, request: Request<Ticket>) -> String {
        let mut stream = match client.do_get(request).await {
            Ok(response) => response.into_inner(),
            Err(status) => return status_line(&status),
        };
        let mut messages = Vec::new();
        loop {
            match stream.message().await {
                Ok(Some(message)) => messages.push(message),
                Ok(None) => break,
                Err(status) => return status_line(&status),
            }
        }
        let parts: Vec<&[u8]> = messages
            .iter()
            .flat_map(|message| {
                [
                    message.data_header.as_ref(),
                    message.app_metadata.as_ref(),
                    message.data_body.as_ref(),
                ]
            })
            .collect();
        let bytes: usize = parts.iter().map(|part| part.len()).sum();
        let batches = arrow_flight::utils::flight_data_to_batches(&messages).expect("decode");
        let rows: usize = batches.iter().map(|batch| batch.num_rows()).sum();
        let columns: Vec<String> = batches
            .first()
            .map(|batch| {
                batch
                    .schema()
                    .fields()
                    .iter()
                    .map(|f| f.name().clone())
                    .collect()
            })
            .unwrap_or_default();
        format!(
            "messages={} bytes={bytes} fnv={:016x} batches={} rows={rows} columns={columns:?}",
            messages.len(),
            fnv1a(&parts),
            batches.len(),
        )
    }

    async fn do_put(client: &mut FlightServiceClient<Channel>, authorized: bool) -> String {
        let request = Request::new(futures::stream::iter(vec![FlightData::default()]));
        let request = if authorized {
            with_valid_bearer(request)
        } else {
            request
        };
        match client.do_put(request).await {
            Ok(_) => "accepted".to_owned(),
            Err(status) => status_line(&status),
        }
    }

    #[tokio::test]
    async fn list_get_info_do_get_and_do_put_answer_as_before_the_shared_server() {
        let root = tempfile::tempdir().expect("tempdir");
        std::fs::write(root.path().join("minimal.docx"), FIXTURE).expect("fixture");
        std::fs::write(root.path().join("notes.txt"), b"not a document").expect("other file");
        std::fs::create_dir(root.path().join("sub")).expect("subdirectory");
        std::fs::write(root.path().join("sub").join("Upper.DOCX"), FIXTURE).expect("nested");
        let mut client = client_for(root.path()).await;
        let descriptor = |path: &str| FlightDescriptor::new_path(vec![path.to_owned()]);
        let ticket = |path: &str| Ticket::new(path.to_owned());

        let mut seen = String::new();
        let mut record = |call: &str, answer: String| {
            writeln!(seen, "{call}\n    {answer}").expect("write to a string");
        };

        record(
            "ListFlights without a bearer",
            list_flights(&mut client, Request::new(Criteria::default())).await,
        );
        record(
            "ListFlights",
            list_flights(
                &mut client,
                with_valid_bearer(Request::new(Criteria::default())),
            )
            .await,
        );
        record(
            "GetFlightInfo without a bearer",
            get_flight_info(&mut client, Request::new(descriptor("minimal.docx"))).await,
        );
        for path in [
            "minimal.docx",
            "minimal.docx?table=metadata",
            "minimal.docx?table=no_such_table",
            "sub/Upper.DOCX",
            "missing.docx",
            "../minimal.docx",
        ] {
            record(
                &format!("GetFlightInfo {path}"),
                get_flight_info(
                    &mut client,
                    with_valid_bearer(Request::new(descriptor(path))),
                )
                .await,
            );
        }
        record(
            "DoGet without a bearer",
            do_get(&mut client, Request::new(ticket("minimal.docx"))).await,
        );
        for path in [
            "minimal.docx",
            "minimal.docx?table=metadata",
            "minimal.docx?table=paragraphs&batch=1",
            "minimal.docx?table=no_such_table",
            "sub/Upper.DOCX",
            "notes.txt",
            "missing.docx",
            "../minimal.docx",
        ] {
            record(
                &format!("DoGet {path}"),
                do_get(&mut client, with_valid_bearer(Request::new(ticket(path)))).await,
            );
        }
        record("DoPut without a bearer", do_put(&mut client, false).await);
        record("DoPut", do_put(&mut client, true).await);

        assert_eq!(seen, EXPECTED, "\n{seen}");
    }
}

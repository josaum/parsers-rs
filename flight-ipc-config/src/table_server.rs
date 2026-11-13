//! One Flight server for parsers that turn a file under a root directory into
//! named Arrow tables.
//!
//! `docx-fast-flight` and `pptx-fast-flight` were the same 626 lines apart
//! from the parser they call, so the server lives here once and each binary
//! says only what differs, through [`TableParser`]. Authentication, the
//! read-only rule, symlink skipping and ticket paths are therefore fixed in
//! one place for both.
use anyhow::anyhow;
use arrow_array::RecordBatch;
use arrow_flight::encode::FlightDataEncoderBuilder;
use arrow_flight::error::FlightError;
use arrow_flight::flight_service_server::{FlightService, FlightServiceServer};
use arrow_flight::{
    Action, ActionType, Criteria, Empty, FlightData, FlightDescriptor, FlightEndpoint, FlightInfo,
    HandshakeRequest, HandshakeResponse, PollInfo, PutResult, SchemaResult, Ticket,
};
use arrow_ipc::writer::IpcWriteOptions;
use arrow_schema::SchemaRef;
use futures::stream::BoxStream;
use futures::StreamExt;
use std::collections::VecDeque;
use std::fs;
use std::path::{Path, PathBuf};
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use tonic::{Request, Response, Status, Streaming};

use crate::{
    capability_json, descriptor_path_and_query, do_put_unsupported, parse_query,
    parse_writer_policies_for_process, resolve_path, root_relative_status, ticket_path_and_query,
    FlightIpcDirection, FlightIpcObservationGuard, FlightRequestAuthVerifier, FlightRpcAction,
    ParserRoute, CAPABILITY_ACTION,
};

/// Everything that differs between two servers built on [`TableFlightService`].
pub trait TableParser: Send + Sync + 'static {
    /// The parser's own parse options.
    type ParseOptions: Copy + Send + Sync + 'static;
    /// The parser's own Arrow export options.
    type ArrowOptions: Clone + Send + Sync + 'static;

    /// The route this server reports for codec policy and telemetry.
    const ROUTE: ParserRoute;
    /// The binary's name, as printed when it starts listening.
    const SERVER_NAME: &'static str;
    /// The address served when `--bind` is not given.
    const DEFAULT_BIND: &'static str;
    /// The table served when a request names none.
    const DEFAULT_TABLE: &'static str;
    /// The status message when [`Self::DEFAULT_TABLE`] has no schema.
    const MISSING_DEFAULT_SCHEMA: &'static str;
    /// The lower-case file extensions ListFlights advertises.
    const EXTENSIONS: &'static [&'static str];

    /// Parse options with the given inflate ceiling and every other default.
    fn parse_options(max_inflate: usize) -> Self::ParseOptions;
    /// Arrow options with the given batch size.
    fn arrow_options(batch_size: usize) -> Self::ArrowOptions;
    /// Apply a request's `max_inflate` override.
    fn set_max_inflate(options: &mut Self::ParseOptions, max_inflate: usize);
    /// Apply a request's `batch` override.
    fn set_batch_size(options: &mut Self::ArrowOptions, batch_size: usize);
    /// Every table name the parser can serve.
    fn list_tables() -> &'static [&'static str];
    /// The schema of one table, or `None` for an unknown name.
    fn schema_for_table(table: &str) -> Option<SchemaRef>;
    /// Parse the file and hand each batch of `table` to `emit`.
    fn parse_table_batches<F>(
        path: &Path,
        parse_options: Self::ParseOptions,
        arrow_options: Self::ArrowOptions,
        table: &str,
        emit: F,
    ) -> anyhow::Result<()>
    where
        F: FnMut(RecordBatch) -> anyhow::Result<()>;

    /// Whether GetSchema looks at its descriptor. A server with one schema
    /// for every file can answer without it, and then answers for a file
    /// that does not exist as well.
    const GET_SCHEMA_READS_DESCRIPTOR: bool = true;

    /// Apply a request's raw query string (empty when there is none) to the
    /// table choice and to this request's copy of the options. It runs before
    /// the path is looked up, so a malformed query is reported first.
    ///
    /// The default understands `table`, `batch`/`batch_size` and
    /// `max_inflate`, and ignores every other key.
    fn apply_query(
        query: &str,
        table: &mut Option<String>,
        parse_options: &mut Self::ParseOptions,
        arrow_options: &mut Self::ArrowOptions,
    ) -> Result<(), Status> {
        let query = parse_query(query)?;
        if let Some(batch_size) = query.batch_size {
            Self::set_batch_size(arrow_options, batch_size);
        }
        if let Some(max_inflate) = query.max_inflate {
            Self::set_max_inflate(parse_options, max_inflate);
        }
        *table = query.table;
        Ok(())
    }
}

/// One request after its query has been applied and its path looked up.
struct Resolved<P: TableParser> {
    path: PathBuf,
    table: String,
    parse_options: P::ParseOptions,
    arrow_options: P::ArrowOptions,
}

/// The Flight service itself. The fields are public so a binary's tests can
/// build one without going through the command line.
pub struct TableFlightService<P: TableParser> {
    pub root: PathBuf,
    pub parse_options: P::ParseOptions,
    pub arrow_options: P::ArrowOptions,
    pub write_options: IpcWriteOptions,
    /// Serialized once at startup; DoAction must not reserialize on every call.
    pub capability_body: Vec<u8>,
    pub auth_verifier: FlightRequestAuthVerifier,
}

impl<P: TableParser> Clone for TableFlightService<P> {
    fn clone(&self) -> Self {
        Self {
            root: self.root.clone(),
            parse_options: self.parse_options,
            arrow_options: self.arrow_options.clone(),
            write_options: self.write_options.clone(),
            capability_body: self.capability_body.clone(),
            auth_verifier: self.auth_verifier.clone(),
        }
    }
}

impl<P: TableParser> TableFlightService<P> {
    fn authorize<T>(&self, request: &Request<T>) -> Result<(), Status> {
        self.auth_verifier.verify_metadata(request.metadata())
    }

    /// Query first, then the path, then the table name: the order in which
    /// a request with several faults has always been answered.
    fn resolve(&self, path: &Path, query: &str) -> Result<Resolved<P>, Status> {
        let mut table = None;
        let mut parse_options = self.parse_options;
        let mut arrow_options = self.arrow_options.clone();
        P::apply_query(query, &mut table, &mut parse_options, &mut arrow_options)?;
        let path = resolve_path(&self.root, path)?;
        if let Some(table) = table.as_deref() {
            if !P::list_tables().contains(&table) {
                return Err(Status::invalid_argument("unknown table"));
            }
        }
        Ok(Resolved {
            path,
            table: table.unwrap_or_else(|| P::DEFAULT_TABLE.to_string()),
            parse_options,
            arrow_options,
        })
    }

    fn resolve_descriptor(&self, descriptor: &FlightDescriptor) -> Result<Resolved<P>, Status> {
        let (path, query) = descriptor_path_and_query(descriptor)?;
        self.resolve(&path, &query)
    }

    fn resolve_ticket(&self, ticket: &Ticket) -> Result<Resolved<P>, Status> {
        let (path, query) = ticket_path_and_query(ticket)?;
        self.resolve(Path::new(path), query)
    }

    /// Parse the file on its own thread and stream the batches of one table.
    fn stream(
        &self,
        request: Resolved<P>,
        observation: FlightIpcObservationGuard,
    ) -> Result<Response<BoxStream<'static, Result<FlightData, Status>>>, Status> {
        let Resolved {
            path,
            table,
            parse_options,
            arrow_options,
        } = request;
        let schema =
            P::schema_for_table(&table).ok_or_else(|| Status::invalid_argument("unknown table"))?;

        let (tx, rx) =
            mpsc::channel::<std::result::Result<arrow_array::RecordBatch, FlightError>>(2);

        std::thread::spawn(move || {
            let result =
                P::parse_table_batches(&path, parse_options, arrow_options, &table, |batch| {
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

        // A parser names the file by its path on the server; the caller
        // only ever sees it the way the ticket spelled it.
        let root = self.root.clone();
        let output = crate::observe_flight_stream(
            flight_stream
                .map(move |res| res.map_err(|err| root_relative_status(to_status(err), &root))),
            observation,
        );
        Ok(Response::new(output))
    }
}

const MAX_LIST_FLIGHTS: usize = 1024;

/// Read the command line and the environment, then serve `P` until the
/// process ends. This is the whole of a server binary's `main`.
pub async fn run<P: TableParser>() -> anyhow::Result<()> {
    let mut bind = P::DEFAULT_BIND.to_string();
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
        P::ROUTE,
    )?
    .authorized_write()?;
    let capability_body =
        serde_json::to_vec(&capability_json(P::ROUTE, Some(authorized_write.codec()))?)?;
    let write_options = authorized_write.into_ipc_write_options();

    let service = TableFlightService::<P> {
        root,
        parse_options: P::parse_options(max_inflate),
        arrow_options: P::arrow_options(batch_size),
        write_options,
        capability_body,
        auth_verifier,
    };

    let addr = bind.parse()?;
    println!("[{}] listening on {addr}", P::SERVER_NAME);

    tonic::transport::Server::builder()
        .add_service(FlightServiceServer::new(service))
        .serve(addr)
        .await?;

    Ok(())
}

#[tonic::async_trait]
impl<P: TableParser> FlightService for TableFlightService<P> {
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
        let schema = P::schema_for_table(P::DEFAULT_TABLE)
            .ok_or_else(|| Status::internal(P::MISSING_DEFAULT_SCHEMA))?;
        let (tx, rx) = mpsc::channel(4);

        std::thread::spawn(move || {
            let root_canon = match root.canonicalize() {
                Ok(path) => path,
                Err(_) => {
                    let _ = tx.blocking_send(Err(Status::internal("failed to resolve root")));
                    return;
                }
            };
            let paths = match collect_paths::<P>(&root_canon, MAX_LIST_FLIGHTS) {
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
        let Resolved {
            path: _path, table, ..
        } = self.resolve_descriptor(&descriptor)?;
        let schema =
            P::schema_for_table(&table).ok_or_else(|| Status::invalid_argument("unknown table"))?;
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
        request: Request<FlightDescriptor>,
    ) -> Result<Response<PollInfo>, Status> {
        self.authorize(&request)?;
        Err(Status::unimplemented("poll_flight_info"))
    }

    async fn get_schema(
        &self,
        request: Request<FlightDescriptor>,
    ) -> Result<Response<SchemaResult>, Status> {
        self.authorize(&request)?;
        let table = if P::GET_SCHEMA_READS_DESCRIPTOR {
            self.resolve_descriptor(&request.into_inner())?.table
        } else {
            P::DEFAULT_TABLE.to_string()
        };
        let schema =
            P::schema_for_table(&table).ok_or_else(|| Status::invalid_argument("unknown table"))?;
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
            P::ROUTE,
            FlightIpcDirection::Read,
            FlightRpcAction::DoGet,
        );
        let request = self.resolve_ticket(&request.into_inner())?;
        self.stream(request, observation)
    }

    async fn do_put(
        &self,
        request: Request<Streaming<FlightData>>,
    ) -> Result<Response<Self::DoPutStream>, Status> {
        // The bearer is checked first: an unauthenticated caller learns
        // nothing about the server, not even that it is read-only.
        self.authorize(&request)?;
        Err(do_put_unsupported())
    }

    async fn do_exchange(
        &self,
        request: Request<Streaming<FlightData>>,
    ) -> Result<Response<Self::DoExchangeStream>, Status> {
        self.authorize(&request)?;
        let observation = FlightIpcObservationGuard::new(
            P::ROUTE,
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

        let request = if let Some(descriptor) = first.flight_descriptor {
            self.resolve_descriptor(&descriptor)?
        } else if !first.app_metadata.is_empty() {
            self.resolve_ticket(&Ticket::new(first.app_metadata))?
        } else {
            return Err(Status::invalid_argument(
                "exchange requires descriptor or app_metadata path",
            ));
        };
        self.stream(request, observation)
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

pub fn collect_paths<P: TableParser>(root: &Path, limit: usize) -> Result<Vec<PathBuf>, Status> {
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
            if file_type.is_file() && has_parser_extension::<P>(&path) {
                out.push(path);
                if out.len() >= limit {
                    return Ok(out);
                }
            }
        }
    }

    Ok(out)
}

fn has_parser_extension<P: TableParser>(path: &Path) -> bool {
    let ext = match path.extension().and_then(|s| s.to_str()) {
        Some(ext) => ext.to_ascii_lowercase(),
        None => return false,
    };
    P::EXTENSIONS.contains(&ext.as_str())
}

fn to_status(err: impl std::fmt::Display) -> Status {
    Status::internal(err.to_string())
}

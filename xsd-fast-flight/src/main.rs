//! Arrow Flight server for xsd-fast-core.
//!
//! Exposes two actions and one `do_get` surface:
//!
//! | Action / Ticket | Payload | Response |
//! |-----------------|---------|----------|
//! | Action `list_elements` | `{"root": "/path/schema.xsd"}` | JSON array of global element names |
//! | Action `describe` | `{"root": "/path/schema.xsd", "element": "name"}` | Arrow IPC (schema description batch) |
//! | Ticket `<path>?element=<name>` | — | Arrow IPC (schema description batch) |
//!
//! `do_exchange` accepts a path + element name and returns the same schema
//! description batch, mirroring the xlsx-fast-flight pattern.
//!
//! ## Usage
//!
//! ```text
//! xsd-fast-flight [--bind 0.0.0.0:50055] [--root /path/to/schemas]
//! ```

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
    capability_json, do_put_unsupported, parse_writer_policies_for_process, split_query,
    FlightIpcDirection, FlightIpcObservationGuard, FlightRequestAuthVerifier, FlightRpcAction,
    ParserRoute, CAPABILITY_ACTION,
};
use futures::stream::BoxStream;
use futures::StreamExt;
use std::collections::VecDeque;
use std::fs;
use std::path::{Path, PathBuf};
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use tonic::{Request, Response, Status, Streaming};
use xsd_fast_core::arrow::ArrowOptions;
use xsd_fast_core::{ParseOptions, SchemaSet};

#[cfg(test)]
#[path = "../../flight-ipc-config/test-support/mod.rs"]
mod flight_auth_test_support;

const MAX_LIST_FLIGHTS: usize = 1024;

#[derive(Clone)]
struct XsdFlightService {
    root: PathBuf,
    parse_options: ParseOptions,
    arrow_options: ArrowOptions,
    write_options: IpcWriteOptions,
    /// Serialized once at startup; DoAction must not reserialize on every call.
    capability_body: Vec<u8>,
    auth_verifier: FlightRequestAuthVerifier,
}

impl XsdFlightService {
    fn authorize<T>(&self, request: &Request<T>) -> Result<(), Status> {
        self.auth_verifier.verify_metadata(request.metadata())
    }
}

#[derive(Debug, Default, Clone)]
struct FlightQuery {
    element: Option<String>,
    max_depth: Option<usize>,
    map_dates: Option<bool>,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let mut bind = "0.0.0.0:50055".to_string();
    let mut root = std::env::current_dir()?;
    let mut max_depth = 8usize;
    let mut map_dates = true;

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
            "--max-depth" => {
                if let Some(v) = args.next() {
                    if let Ok(d) = v.parse() {
                        max_depth = d;
                    }
                }
            }
            "--no-dates" => map_dates = false,
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
        ParserRoute::Xsd,
    )?
    .authorized_write()?;
    let capability_body = serde_json::to_vec(&capability_json(
        ParserRoute::Xsd,
        Some(authorized_write.codec()),
    )?)?;
    let write_options = authorized_write.into_ipc_write_options();

    let service = XsdFlightService {
        root,
        parse_options: ParseOptions {
            max_includes: 64,
            map_date_types: map_dates,
            ..ParseOptions::default()
        },
        arrow_options: ArrowOptions {
            max_depth,
            map_date_types: map_dates,
            include_attributes: true,
            batch_size: 8_192,
        },
        write_options,
        capability_body,
        auth_verifier,
    };

    let addr = bind.parse()?;
    println!("[xsd-fast-flight] listening on {addr}");

    tonic::transport::Server::builder()
        .add_service(FlightServiceServer::new(service))
        .serve(addr)
        .await?;

    Ok(())
}

#[tonic::async_trait]
impl FlightService for XsdFlightService {
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
        let (tx, rx) = mpsc::channel(4);

        std::thread::spawn(move || {
            let root_canon = match root.canonicalize() {
                Ok(p) => p,
                Err(_) => {
                    let _ = tx.blocking_send(Err(Status::internal("failed to resolve root")));
                    return;
                }
            };
            let paths = match collect_xsd_paths(&root_canon, MAX_LIST_FLIGHTS) {
                Ok(list) => list,
                Err(err) => {
                    let _ = tx.blocking_send(Err(err));
                    return;
                }
            };
            for path in paths {
                let rel = match path.strip_prefix(&root_canon) {
                    Ok(r) => r,
                    Err(_) => continue,
                };
                let components = path_components(rel);
                let rel_str = components.join("/");
                let ticket = Ticket::new(rel_str);
                let descriptor = FlightDescriptor::new_path(components);
                let endpoint = FlightEndpoint::new().with_ticket(ticket);

                // Use a generic schema for listing (no element name yet).
                let list_schema = description_schema();
                let mut info = match FlightInfo::new().try_with_schema(list_schema.as_ref()) {
                    Ok(i) => i,
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
        let ticket = Ticket::new(path_to_ticket(&self.root, &path, &query));
        let schema = description_schema();
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
        let schema = description_schema();
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
        let ticket = request.into_inner();
        let (path, query) = ticket_to_request(&self.root, &ticket)?;
        let opts = self.arrow_options.clone();
        let parse_opts = self.parse_options.clone();

        stream_schema_description(
            path,
            query,
            parse_opts,
            opts,
            self.write_options.clone(),
            FlightRpcAction::DoGet,
        )
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

        stream_schema_description(
            path,
            query,
            self.parse_options.clone(),
            self.arrow_options.clone(),
            self.write_options.clone(),
            FlightRpcAction::DoExchange,
        )
    }

    async fn do_action(
        &self,
        request: Request<Action>,
    ) -> Result<Response<Self::DoActionStream>, Status> {
        self.authorize(&request)?;
        let action = request.into_inner();
        if action.r#type == CAPABILITY_ACTION {
            let (tx, rx) = mpsc::channel(1);
            tx.send(Ok(arrow_flight::Result {
                body: self.capability_body.clone().into(),
            }))
            .await
            .map_err(|_| Status::internal("capability response failed"))?;
            return Ok(Response::new(ReceiverStream::new(rx)));
        }
        let (tx, rx) = mpsc::channel(4);
        let root = self.root.clone();
        let parse_opts = self.parse_options.clone();

        match action.r#type.as_str() {
            "list_elements" => {
                let body = std::str::from_utf8(&action.body)
                    .map_err(|_| Status::invalid_argument("body must be utf-8"))?
                    .to_string();
                let path_str = parse_action_path(&body)?;
                let path = resolve_path(&root, Path::new(&path_str))?;

                std::thread::spawn(move || {
                    let result = (|| -> anyhow::Result<Vec<u8>> {
                        let schema_set = SchemaSet::load(&path, parse_opts)?;
                        let names: Vec<&str> = schema_set
                            .model()
                            .global_elements
                            .keys()
                            .map(|s| s.as_str())
                            .collect();
                        Ok(serde_json::to_vec(&names)?)
                    })();
                    match result {
                        Ok(bytes) => {
                            let _ =
                                tx.blocking_send(Ok(arrow_flight::Result { body: bytes.into() }));
                        }
                        Err(e) => {
                            let _ = tx.blocking_send(Err(Status::internal(e.to_string())));
                        }
                    }
                });
            }
            "describe" => {
                let body = std::str::from_utf8(&action.body)
                    .map_err(|_| Status::invalid_argument("body must be utf-8"))?
                    .to_string();
                let (path_str, element) = parse_action_describe(&body)?;
                let path = resolve_path(&root, Path::new(&path_str))?;
                let arrow_opts = self.arrow_options.clone();
                let write_options = self.write_options.clone();

                std::thread::spawn(move || {
                    let result = (|| -> anyhow::Result<Vec<u8>> {
                        let schema_set = SchemaSet::load(&path, parse_opts)?;
                        let mut buf = Vec::new();
                        schema_set.write_schema_ipc_with_options(
                            &element,
                            arrow_opts,
                            write_options,
                            &mut buf,
                        )?;
                        Ok(buf)
                    })();
                    match result {
                        Ok(bytes) => {
                            let _ =
                                tx.blocking_send(Ok(arrow_flight::Result { body: bytes.into() }));
                        }
                        Err(e) => {
                            let _ = tx.blocking_send(Err(Status::internal(e.to_string())));
                        }
                    }
                });
            }
            other => {
                return Err(Status::unimplemented(format!("unknown action: {other}")));
            }
        }

        Ok(Response::new(ReceiverStream::new(rx)))
    }

    async fn list_actions(
        &self,
        request: Request<Empty>,
    ) -> Result<Response<Self::ListActionsStream>, Status> {
        self.authorize(&request)?;
        let (tx, rx) = mpsc::channel(4);
        let actions = vec![
            ActionType {
                r#type: "list_elements".to_string(),
                description: "List global element names for an XSD file".to_string(),
            },
            ActionType {
                r#type: "describe".to_string(),
                description: "Derive Arrow schema description for a root element".to_string(),
            },
            ActionType {
                r#type: CAPABILITY_ACTION.to_string(),
                description: "cached codec capabilities".to_string(),
            },
        ];
        tokio::spawn(async move {
            for at in actions {
                if tx.send(Ok(at)).await.is_err() {
                    break;
                }
            }
        });
        Ok(Response::new(ReceiverStream::new(rx)))
    }
}

// ── Streaming helper ─────────────────────────────────────────────────────────

fn stream_schema_description(
    path: PathBuf,
    query: FlightQuery,
    parse_opts: ParseOptions,
    arrow_opts: ArrowOptions,
    write_options: IpcWriteOptions,
    action: FlightRpcAction,
) -> Result<Response<BoxStream<'static, Result<FlightData, Status>>>, Status> {
    let observation =
        FlightIpcObservationGuard::new(ParserRoute::Xsd, FlightIpcDirection::Read, action);
    let element = query
        .element
        .ok_or_else(|| Status::invalid_argument("ticket must specify ?element=<name>"))?;
    let mut opts = arrow_opts;
    if let Some(d) = query.max_depth {
        opts.max_depth = d;
    }
    if let Some(m) = query.map_dates {
        opts.map_date_types = m;
    }

    let schema = description_schema();
    let (tx, rx) = mpsc::channel::<std::result::Result<arrow_array::RecordBatch, FlightError>>(2);

    std::thread::spawn(move || {
        let result = (|| -> anyhow::Result<()> {
            let schema_set = SchemaSet::load(&path, parse_opts)?;
            let batch = schema_set.to_schema_description_batch(&element, opts)?;
            tx.blocking_send(Ok(batch))
                .map_err(|_| anyhow!("flight client dropped"))?;
            Ok(())
        })();
        if let Err(err) = result {
            let io_err = std::io::Error::other(err);
            let _ = tx.blocking_send(Err(FlightError::from_external_error(Box::new(io_err))));
        }
    });

    let batch_stream = ReceiverStream::new(rx);
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

// ── Schema for description batches ───────────────────────────────────────────

fn description_schema() -> std::sync::Arc<arrow_schema::Schema> {
    use arrow_schema::{DataType, Field, Schema};
    use std::sync::Arc;
    Arc::new(Schema::new(vec![
        Field::new("path", DataType::Utf8, false),
        Field::new("arrow_type", DataType::Utf8, false),
        Field::new("nullable", DataType::Utf8, false),
    ]))
}

// ── Path helpers ──────────────────────────────────────────────────────────────

fn descriptor_to_request(
    root: &Path,
    descriptor: &FlightDescriptor,
) -> Result<(PathBuf, FlightQuery), Status> {
    if !descriptor.path.is_empty() {
        let mut parts = descriptor.path.clone();
        let mut query = FlightQuery::default();
        if let Some(last) = parts.last_mut() {
            let last_val = last.clone();
            if let Some((base, q)) = split_query(&last_val) {
                *last = base.to_string();
                query = parse_query(q)?;
            }
        }
        let path = parts.iter().fold(PathBuf::new(), |mut buf, part| {
            buf.push(part);
            buf
        });
        resolve_path(root, &path).map(|p| (p, query))
    } else if !descriptor.cmd.is_empty() {
        let cmd = std::str::from_utf8(&descriptor.cmd)
            .map_err(|_| Status::invalid_argument("descriptor cmd must be utf-8"))?;
        let (path_str, q_str) = split_query(cmd).unwrap_or((cmd, ""));
        let query = if q_str.is_empty() {
            FlightQuery::default()
        } else {
            parse_query(q_str)?
        };
        resolve_path(root, Path::new(path_str)).map(|p| (p, query))
    } else {
        Err(Status::invalid_argument("missing flight descriptor path"))
    }
}

fn ticket_to_request(root: &Path, ticket: &Ticket) -> Result<(PathBuf, FlightQuery), Status> {
    let path = std::str::from_utf8(&ticket.ticket)
        .map_err(|_| Status::invalid_argument("ticket must be utf-8"))?;
    let (path_str, q_str) = split_query(path).unwrap_or((path, ""));
    let query = if q_str.is_empty() {
        FlightQuery::default()
    } else {
        parse_query(q_str)?
    };
    resolve_path(root, Path::new(path_str)).map(|p| (p, query))
}

fn resolve_path(root: &Path, path: &Path) -> Result<PathBuf, Status> {
    let joined = if path.is_absolute() {
        path.to_path_buf()
    } else {
        root.join(path)
    };
    let canon = joined
        .canonicalize()
        .map_err(|_| Status::not_found("XSD file not found"))?;
    let root_canon = root
        .canonicalize()
        .map_err(|_| Status::internal("failed to resolve root"))?;
    if !canon.starts_with(&root_canon) {
        return Err(Status::permission_denied("path escapes root"));
    }
    Ok(canon)
}

fn path_to_ticket(root: &Path, path: &Path, query: &FlightQuery) -> String {
    let rel = if let Ok(rc) = root.canonicalize() {
        if let Ok(r) = path.strip_prefix(&rc) {
            path_components(r).join("/")
        } else {
            path.to_string_lossy().to_string()
        }
    } else {
        path.to_string_lossy().to_string()
    };
    if let Some(el) = &query.element {
        format!("{rel}?element={el}")
    } else {
        rel
    }
}

fn path_components(path: &Path) -> Vec<String> {
    path.components()
        .map(|c| c.as_os_str().to_string_lossy().to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

fn collect_xsd_paths(root: &Path, limit: usize) -> Result<Vec<PathBuf>, Status> {
    let mut out = Vec::new();
    let mut queue = VecDeque::new();
    queue.push_back(root.to_path_buf());
    while let Some(dir) = queue.pop_front() {
        let entries = match fs::read_dir(&dir) {
            Ok(e) => e,
            Err(_) => continue,
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let ft = match entry.file_type() {
                Ok(ft) => ft,
                Err(_) => continue,
            };
            if ft.is_symlink() {
                continue;
            }
            if ft.is_dir() {
                queue.push_back(path);
                continue;
            }
            if ft.is_file()
                && path
                    .extension()
                    .and_then(|s| s.to_str())
                    .is_some_and(|e| e.eq_ignore_ascii_case("xsd"))
            {
                out.push(path);
                if out.len() >= limit {
                    return Ok(out);
                }
            }
        }
    }
    Ok(out)
}

fn parse_query(q: &str) -> Result<FlightQuery, Status> {
    let mut fq = FlightQuery::default();
    for part in q.split('&') {
        let mut kv = part.splitn(2, '=');
        let key = kv.next().unwrap_or("");
        let val = kv.next().unwrap_or("");
        match key {
            "element" => fq.element = Some(val.to_string()),
            "max_depth" => fq.max_depth = val.parse().ok(),
            "map_dates" => fq.map_dates = Some(val == "true" || val == "1"),
            _ => {}
        }
    }
    Ok(fq)
}

fn parse_action_path(body: &str) -> Result<String, Status> {
    // Accept either `{"root": "..."}` JSON or bare path string.
    if body.trim_start().starts_with('{') {
        let v: serde_json::Value = serde_json::from_str(body)
            .map_err(|e| Status::invalid_argument(format!("JSON parse error: {e}")))?;
        v["root"]
            .as_str()
            .map(|s| s.to_string())
            .ok_or_else(|| Status::invalid_argument("missing 'root' field in action body"))
    } else {
        Ok(body.trim().to_string())
    }
}

fn parse_action_describe(body: &str) -> Result<(String, String), Status> {
    let v: serde_json::Value = serde_json::from_str(body)
        .map_err(|e| Status::invalid_argument(format!("JSON parse error: {e}")))?;
    let root = v["root"]
        .as_str()
        .ok_or_else(|| Status::invalid_argument("missing 'root' field"))?
        .to_string();
    let element = v["element"]
        .as_str()
        .ok_or_else(|| Status::invalid_argument("missing 'element' field"))?
        .to_string();
    Ok((root, element))
}

fn to_status(err: impl std::fmt::Display) -> Status {
    Status::internal(err.to_string())
}

// serde_json is needed for action body parsing.

#[cfg(test)]
mod symlink_listing_tests {
    use super::*;
    #[cfg(unix)]
    use std::os::unix::fs::symlink as symlink_file;
    #[cfg(windows)]
    use std::os::windows::fs::symlink_file;

    #[test]
    fn collect_xsd_paths_skips_symlinks() {
        // WHY: listing must not follow symlinks that could escape the Flight
        // root; canonicalize on DoGet is not enough if ListFlights advertises
        // the linked path.
        let root = tempfile::tempdir().expect("tempdir");
        let outside = tempfile::tempdir().expect("outside");
        let real = outside.path().join("escape.xsd");
        std::fs::write(&real, b"<xs:schema/>").expect("write");
        let link = root.path().join("escape.xsd");
        if let Err(err) = symlink_file(&real, &link) {
            #[cfg(windows)]
            if err.raw_os_error() == Some(1314) {
                return;
            }
            panic!("symlink: {err}");
        }
        let inside = root.path().join("ok.xsd");
        std::fs::write(&inside, b"<xs:schema/>").expect("write inside");

        let found = collect_xsd_paths(root.path(), 100).expect("collect");
        assert_eq!(found.len(), 1);
        assert_eq!(found[0], inside);
    }
}

#[cfg(test)]
mod read_only_flight_contract_tests {
    use super::*;
    use crate::flight_auth_test_support::{required_auth_verifier, with_valid_bearer};

    fn capability_test_service() -> XsdFlightService {
        XsdFlightService {
            root: PathBuf::new(),
            parse_options: ParseOptions {
                max_includes: 64,
                map_date_types: true,
                ..ParseOptions::default()
            },
            arrow_options: ArrowOptions {
                max_depth: 8,
                map_date_types: true,
                include_attributes: true,
                batch_size: 64,
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
    async fn non_capability_actions_are_not_reclassified_as_auth_failures() {
        let service = capability_test_service();
        let Err(error) = FlightService::do_action(
            &service,
            with_valid_bearer(Request::new(Action {
                r#type: "unknown".to_owned(),
                body: Default::default(),
            })),
        )
        .await
        else {
            panic!("unknown action must remain unsupported");
        };
        assert_eq!(error.code(), tonic::Code::Unimplemented);
    }

    #[tokio::test]
    async fn retrieval_and_domain_action_reject_auth_before_payload_parsing() {
        let service = capability_test_service();
        let schema_result = FlightService::get_schema(
            &service,
            Request::new(FlightDescriptor::new_path(vec![
                "__missing_auth_probe__.xsd".to_owned(),
            ])),
        )
        .await;
        let Err(schema_error) = schema_result else {
            panic!("unauthenticated retrieval must fail");
        };
        assert_eq!(schema_error.code(), tonic::Code::Unauthenticated);

        let action_result = FlightService::do_action(
            &service,
            Request::new(Action {
                r#type: "list_elements".to_owned(),
                body: b"not-json".to_vec().into(),
            }),
        )
        .await;
        let Err(action_error) = action_result else {
            panic!("unauthenticated domain action must fail");
        };
        assert_eq!(action_error.code(), tonic::Code::Unauthenticated);
    }

    #[tokio::test]
    async fn valid_bearer_reaches_retrieval_dispatch() {
        let service = capability_test_service();
        let request = with_valid_bearer(Request::new(FlightDescriptor::new_path(vec![
            "__missing_auth_probe__.xsd".to_owned(),
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
    //! own copy of the Flight skeleton and of the path and ticket handling, so
    //! a change to any answer in it is a change in behaviour, not in
    //! structure. Unix only: the probes include a symbolic link and a
    //! backslash in a file name.
    use super::*;
    use crate::flight_auth_test_support::required_auth_verifier;
    use crate::flight_wire_test_support::Wire;

    const EXPECTED: &str = include_str!("../tests/wire_transcript.txt");

    const FIXTURE: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<xs:schema xmlns:xs="http://www.w3.org/2001/XMLSchema">
  <xs:element name="order">
    <xs:complexType>
      <xs:sequence>
        <xs:element name="id" type="xs:int"/>
        <xs:element name="placed" type="xs:date" minOccurs="0"/>
        <xs:element name="item" maxOccurs="unbounded">
          <xs:complexType>
            <xs:sequence>
              <xs:element name="sku" type="xs:string"/>
              <xs:element name="quantity" type="xs:int"/>
            </xs:sequence>
          </xs:complexType>
        </xs:element>
      </xs:sequence>
      <xs:attribute name="status" type="xs:string"/>
    </xs:complexType>
  </xs:element>
</xs:schema>
"#;

    #[tokio::test]
    async fn every_rpc_answers_as_it_did_before_the_shared_server() {
        let base = tempfile::tempdir().expect("tempdir");
        let root = base.path().join("root");
        std::fs::create_dir_all(root.join("sub")).expect("directories");
        std::fs::write(root.join("order.xsd"), FIXTURE).expect("fixture");
        std::fs::write(root.join("sub").join("Upper.XSD"), FIXTURE).expect("nested");
        std::fs::write(root.join("notes.txt"), b"not a schema").expect("other file");
        std::fs::write(base.path().join("outside.xsd"), FIXTURE).expect("outside");
        std::os::unix::fs::symlink(base.path().join("outside.xsd"), root.join("link.xsd"))
            .expect("symlink");
        let outside = base.path().join("outside.xsd");
        let outside = outside.to_string_lossy();
        let outside_order = format!("{outside}?element=order");

        let service = XsdFlightService {
            root: root.clone(),
            parse_options: ParseOptions {
                max_includes: 64,
                map_date_types: true,
                ..ParseOptions::default()
            },
            arrow_options: ArrowOptions {
                max_depth: 8,
                map_date_types: true,
                include_attributes: true,
                batch_size: 64,
            },
            write_options: IpcWriteOptions::default(),
            capability_body: b"{}".to_vec(),
            auth_verifier: required_auth_verifier(),
        };
        let mut wire = Wire::start(service, base.path()).await;

        wire.handshake(false).await;
        wire.handshake(true).await;
        wire.list_flights(false).await;
        wire.list_flights(true).await;

        wire.get_flight_info("order.xsd", false).await;
        for path in [
            "order.xsd",
            "order.xsd?element=order",
            "order.xsd?element=order&max_depth=1&table=x",
            "order.xsd?element=a%20b&element=c",
            "order.xsd?",
            "sub/Upper.XSD",
            "sub\\Upper.XSD",
            "notes.txt",
            "missing.xsd",
            "../outside.xsd",
            "link.xsd",
            outside.as_ref(),
        ] {
            wire.get_flight_info(path, true).await;
        }
        wire.get_flight_info_parts(&["sub", "Upper.XSD?element=order"], true)
            .await;
        wire.get_flight_info_parts(&["sub", "..", "order.xsd"], true)
            .await;
        wire.get_flight_info_cmd(b"order.xsd?element=order", true)
            .await;
        wire.get_flight_info_cmd(b"\xff", true).await;
        wire.get_flight_info_cmd(b"", true).await;

        wire.poll_flight_info("order.xsd", false).await;
        wire.poll_flight_info("order.xsd", true).await;

        wire.get_schema("order.xsd", false).await;
        wire.get_schema("order.xsd", true).await;
        wire.get_schema("missing.xsd", true).await;

        wire.do_get(b"order.xsd?element=order", false).await;
        for ticket in [
            "order.xsd",
            "order.xsd?element=order",
            "order.xsd?element=nope",
            "order.xsd?element=order&max_depth=0",
            "order.xsd?element=order&max_depth=x&map_dates=0",
            "order.xsd?element=order&map_dates=true&batch=1",
            "sub/Upper.XSD?element=order",
            "sub\\Upper.XSD?element=order",
            "notes.txt?element=order",
            "missing.xsd?element=order",
            "missing.xsd",
            "../outside.xsd?element=order",
            "sub/../order.xsd?element=order",
            "link.xsd?element=order",
            outside_order.as_str(),
            "",
        ] {
            wire.do_get(ticket.as_bytes(), true).await;
        }
        wire.do_get(b"\xff", true).await;

        wire.do_put(false).await;
        wire.do_put(true).await;

        wire.do_exchange_path("order.xsd?element=order", false)
            .await;
        wire.do_exchange_path("order.xsd?element=order", true).await;
        wire.do_exchange_path("order.xsd", true).await;
        wire.do_exchange_path("missing.xsd?element=order", true)
            .await;
        wire.do_exchange_metadata("order.xsd?element=order&max_depth=1", true)
            .await;
        wire.do_exchange_metadata("../outside.xsd?element=order", true)
            .await;
        wire.do_exchange("an empty message", vec![FlightData::default()], true)
            .await;
        wire.do_exchange("no message", Vec::new(), true).await;

        wire.do_action(CAPABILITY_ACTION, "", true, false).await;
        wire.do_action(CAPABILITY_ACTION, "", true, true).await;
        wire.do_action("list_elements", r#"{"root":"order.xsd"}"#, true, false)
            .await;
        for body in [
            r#"{"root":"order.xsd"}"#,
            "  order.xsd ",
            r#"{"root":"sub/Upper.XSD"}"#,
            r#"{"path":"order.xsd"}"#,
            "{bad",
            r#"{"root":"notes.txt"}"#,
            r#"{"root":"missing.xsd"}"#,
            r#"{"root":"../outside.xsd"}"#,
            r#"{"root":"link.xsd"}"#,
            "",
        ] {
            wire.do_action("list_elements", body, true, true).await;
        }
        wire.do_action(
            "describe",
            r#"{"root":"order.xsd","element":"order"}"#,
            false,
            false,
        )
        .await;
        for body in [
            r#"{"root":"order.xsd","element":"order"}"#,
            r#"{"root":"order.xsd","element":"nope"}"#,
            r#"{"root":"order.xsd"}"#,
            r#"{"element":"order"}"#,
            "order.xsd",
            r#"{"root":"missing.xsd","element":"order"}"#,
            r#"{"root":"../outside.xsd","element":"order"}"#,
        ] {
            wire.do_action("describe", body, false, true).await;
        }
        wire.do_action("unknown", "", true, true).await;
        wire.list_actions(false).await;
        wire.list_actions(true).await;

        assert_eq!(wire.seen, EXPECTED, "\n<<<WIRE\n{}WIRE>>>", wire.seen);
    }
}

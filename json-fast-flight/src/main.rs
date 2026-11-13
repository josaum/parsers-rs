//! `json-fast-flight` — Arrow Flight server for JSON / JSONL.
//!
//! Serves files from a root directory as Apache Arrow `RecordBatch`es. Each
//! ticket / descriptor is a path relative to `--root` (`.json` or `.jsonl`),
//! optionally with a `?batch_size=N` query. The schema is inferred per file by
//! `json-fast-core`'s `arrow` feature; nested values become JSON-string
//! columns.
//!
//! Reader-only: `DoPut` / `DoExchange` are unimplemented and the capability
//! action advertises no writer route (mirrors `email-fast-flight`). Auth is
//! the shared `flight-ipc-config` verifier (env-driven), and every `DoGet`
//! stream carries IPC telemetry observation.
//!
//! ```
//! json-fast-flight --bind 0.0.0.0:50053 --root ./data --batch-size 16384
//! ```

use arrow_flight::encode::FlightDataEncoderBuilder;
use arrow_flight::error::FlightError;
use arrow_flight::flight_service_server::{FlightService, FlightServiceServer};
use arrow_flight::{
    Action, ActionType, Criteria, Empty, FlightData, FlightDescriptor, FlightEndpoint, FlightInfo,
    HandshakeRequest, HandshakeResponse, PollInfo, PutResult, SchemaResult, Ticket,
};
use arrow_ipc::writer::IpcWriteOptions;
use arrow_schema::SchemaRef;
use flight_ipc_config::{
    capability_json, FlightIpcDirection, FlightIpcObservationGuard, FlightRequestAuthVerifier,
    FlightRpcAction, ParserRoute, CAPABILITY_ACTION,
};
use futures::stream::BoxStream;
use futures::StreamExt;
use json_fast_core::arrow::{self, ArrowOptions};
use std::collections::VecDeque;
use std::fs;
use std::path::{Path, PathBuf};
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use tonic::{Request, Response, Status, Streaming};

#[derive(Clone)]
struct JsonFlightService {
    root: PathBuf,
    arrow_options: ArrowOptions,
    write_options: IpcWriteOptions,
    /// Serialized once at startup; DoAction must not reserialize per call.
    capability_body: Vec<u8>,
    auth_verifier: FlightRequestAuthVerifier,
}

impl JsonFlightService {
    fn authorize<T>(&self, request: &Request<T>) -> Result<(), Status> {
        self.auth_verifier.verify_metadata(request.metadata())
    }
}

/// Query parameters accepted on a ticket / descriptor path.
#[derive(Default, Debug, Clone)]
struct FlightQuery {
    batch_size: Option<usize>,
}

const MAX_LIST_FLIGHTS: usize = 1024;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let mut bind = "0.0.0.0:50053".to_string();
    let mut root = std::env::current_dir()?;
    let mut batch_size = 16_384usize;

    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--bind" => {
                if let Some(value) = args.next() {
                    bind = value;
                }
            }
            "--root" => {
                if let Some(value) = args.next() {
                    root = PathBuf::from(value);
                }
            }
            "--batch-size" => {
                if let Some(value) = args.next() {
                    if let Ok(parsed) = value.parse::<usize>() {
                        batch_size = parsed;
                    }
                }
            }
            _ => {}
        }
    }

    let auth_verifier = FlightRequestAuthVerifier::from_env_required()?;
    let capability_body = serde_json::to_vec(&capability_json(ParserRoute::Json, None)?)?;

    let service = JsonFlightService {
        root,
        arrow_options: ArrowOptions {
            batch_size,
            schema: None,
        },
        write_options: IpcWriteOptions::default(),
        capability_body,
        auth_verifier,
    };

    let addr = bind.parse()?;
    println!("[json-fast-flight] listening on {addr}");

    tonic::transport::Server::builder()
        .add_service(FlightServiceServer::new(service))
        .serve(addr)
        .await?;

    Ok(())
}

fn to_status(err: impl std::fmt::Display) -> Status {
    Status::internal(err.to_string())
}

fn parse_query(raw: &str) -> Result<FlightQuery, Status> {
    let mut query = FlightQuery::default();
    for pair in raw.split('&').filter(|p| !p.is_empty()) {
        let Some((key, value)) = pair.split_once('=') else {
            return Err(Status::invalid_argument(format!(
                "invalid query pair: {pair}"
            )));
        };
        match key {
            "batch_size" => {
                query.batch_size = Some(
                    value
                        .parse::<usize>()
                        .map_err(|_| Status::invalid_argument("batch_size must be a number"))?,
                );
            }
            other => {
                return Err(Status::invalid_argument(format!(
                    "unsupported query parameter: {other}"
                )));
            }
        }
    }
    Ok(query)
}

fn split_query(input: &str) -> Option<(&str, &str)> {
    input.split_once('?')
}

fn ensure_query_supported(query: &FlightQuery) -> Result<(), Status> {
    // Only batch_size exists today; anything else was rejected by parse_query.
    let _ = query;
    Ok(())
}

fn apply_query_overrides(query: &FlightQuery, options: &mut ArrowOptions) {
    if let Some(size) = query.batch_size {
        options.batch_size = size;
    }
}

fn is_json_file(path: &Path) -> bool {
    matches!(
        path.extension().and_then(|e| e.to_str()),
        Some("json") | Some("jsonl")
    )
}

fn collect_json_paths(root: &Path, limit: usize) -> Result<Vec<PathBuf>, Status> {
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
            if file_type.is_file() && is_json_file(&path) {
                out.push(path);
                if out.len() >= limit {
                    return Ok(out);
                }
            }
        }
    }

    Ok(out)
}

fn resolve_path(root: &Path, path: &Path) -> Result<PathBuf, Status> {
    let joined = if path.is_absolute() {
        path.to_path_buf()
    } else {
        root.join(path)
    };
    let canon = joined
        .canonicalize()
        .map_err(|_| Status::not_found("file not found"))?;
    let root_canon = root
        .canonicalize()
        .map_err(|_| Status::internal("failed to resolve root"))?;
    if !canon.starts_with(&root_canon) {
        return Err(Status::permission_denied("path escapes root"));
    }
    Ok(canon)
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

fn file_schema(path: &Path, options: &ArrowOptions) -> Result<SchemaRef, Status> {
    let text = fs::read_to_string(path)
        .map_err(|err| Status::internal(format!("read {}: {err}", path.display())))?;
    arrow::jsonl_schema(&text, options).map_err(|err| Status::internal(err.to_string()))
}

#[tonic::async_trait]
impl FlightService for JsonFlightService {
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
        let options = self.arrow_options.clone();
        let (tx, rx) = mpsc::channel(4);

        std::thread::spawn(move || {
            let root_canon = match root.canonicalize() {
                Ok(path) => path,
                Err(_) => {
                    let _ = tx.blocking_send(Err(Status::internal("failed to resolve root")));
                    return;
                }
            };
            let paths = match collect_json_paths(&root_canon, MAX_LIST_FLIGHTS) {
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

                let schema = match file_schema(&path, &options) {
                    Ok(schema) => schema,
                    Err(err) => {
                        let _ = tx.blocking_send(Err(err));
                        return;
                    }
                };
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

        let mut options = self.arrow_options.clone();
        apply_query_overrides(&query, &mut options);
        let schema = file_schema(&path, &options)?;

        let ticket = Ticket::new(path_to_ticket(&self.root, &path));
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
        let descriptor = request.into_inner();
        let (path, query) = descriptor_to_request(&self.root, &descriptor)?;
        ensure_query_supported(&query)?;

        let mut options = self.arrow_options.clone();
        apply_query_overrides(&query, &mut options);
        let schema = file_schema(&path, &options)?;

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
            ParserRoute::Json,
            FlightIpcDirection::Read,
            FlightRpcAction::DoGet,
        );
        let ticket = request.into_inner();
        let (path, query) = ticket_to_request(&self.root, &ticket)?;
        ensure_query_supported(&query)?;

        let mut options = self.arrow_options.clone();
        apply_query_overrides(&query, &mut options);

        let text = fs::read_to_string(&path)
            .map_err(|err| Status::internal(format!("read {}: {err}", path.display())))?;
        let schema = arrow::jsonl_schema(&text, &options).map_err(to_status)?;

        let (tx, rx) =
            mpsc::channel::<std::result::Result<arrow_array::RecordBatch, FlightError>>(2);

        std::thread::spawn(move || {
            let result = arrow::parse_jsonl_to_arrow_batches(&text, &options, |batch| {
                tx.blocking_send(Ok(batch)).map_err(|_| {
                    json_fast_core::JsonError::Structure("flight client dropped".to_string())
                })?;
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
        Err(Status::unimplemented(
            "json-fast-flight is reader-only (use json-fast-zerobus for writes)",
        ))
    }

    async fn do_exchange(
        &self,
        _request: Request<Streaming<FlightData>>,
    ) -> Result<Response<Self::DoExchangeStream>, Status> {
        Err(Status::unimplemented("do_exchange"))
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

#[cfg(test)]
#[path = "../../flight-ipc-config/test-support/mod.rs"]
mod flight_auth_test_support;

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

    const FIXTURE: &str = "{\"id\":1,\"name\":\"ada\",\"tags\":[\"a\"]}\n\
{\"id\":2,\"name\":\"bob\",\"score\":1.5}\n\
{\"id\":3,\"name\":null}\n";

    #[tokio::test]
    async fn every_rpc_answers_as_it_did_before_the_shared_server() {
        let base = tempfile::tempdir().expect("tempdir");
        let root = base.path().join("root");
        std::fs::create_dir_all(root.join("sub")).expect("directories");
        std::fs::write(root.join("data.jsonl"), FIXTURE).expect("fixture");
        std::fs::write(root.join("Upper.JSON"), FIXTURE).expect("upper-case extension");
        std::fs::write(root.join("sub").join("nested.json"), "{\"only\":true}\n").expect("nested");
        std::fs::write(root.join("notes.txt"), b"not json").expect("other file");
        std::fs::write(base.path().join("outside.jsonl"), FIXTURE).expect("outside");
        std::os::unix::fs::symlink(base.path().join("outside.jsonl"), root.join("link.jsonl"))
            .expect("symlink");
        let outside = base.path().join("outside.jsonl");
        let outside = outside.to_string_lossy();

        let service = JsonFlightService {
            root: root.clone(),
            arrow_options: ArrowOptions {
                batch_size: 64,
                schema: None,
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

        wire.get_flight_info("data.jsonl", false).await;
        for path in [
            "data.jsonl",
            "data.jsonl?batch_size=1",
            "data.jsonl?batch=1",
            "data.jsonl?table=x",
            "data.jsonl?batch_size",
            "data.jsonl?batch_size=x",
            "data.jsonl?batch_size=0",
            "data.jsonl?batch_size=%31",
            "data.jsonl?",
            "Upper.JSON",
            "sub/nested.json",
            "sub\\nested.json",
            "notes.txt",
            "sub",
            "missing.jsonl",
            "missing.jsonl?batch=1",
            "../outside.jsonl",
            "link.jsonl",
            outside.as_ref(),
        ] {
            wire.get_flight_info(path, true).await;
        }
        wire.get_flight_info_parts(&["sub", "nested.json?batch_size=2"], true)
            .await;
        wire.get_flight_info_parts(&["sub", "..", "data.jsonl"], true)
            .await;
        wire.get_flight_info_cmd(b"data.jsonl?batch_size=2", true)
            .await;
        wire.get_flight_info_cmd(b"data.jsonl?batch=2", true).await;
        wire.get_flight_info_cmd(b"\xff", true).await;
        wire.get_flight_info_cmd(b"", true).await;

        wire.poll_flight_info("data.jsonl", false).await;
        wire.poll_flight_info("data.jsonl", true).await;

        wire.get_schema("data.jsonl", false).await;
        wire.get_schema("data.jsonl", true).await;
        wire.get_schema("sub/nested.json", true).await;
        wire.get_schema("missing.jsonl", true).await;
        wire.get_schema("notes.txt", true).await;

        wire.do_get(b"data.jsonl", false).await;
        for ticket in [
            "data.jsonl",
            "data.jsonl?batch_size=1",
            "data.jsonl?batch=1",
            "data.jsonl?batch_size=x",
            "data.jsonl?",
            "Upper.JSON",
            "sub/nested.json",
            "sub\\nested.json",
            "notes.txt",
            "sub",
            "missing.jsonl",
            "../outside.jsonl",
            "sub/../data.jsonl",
            "link.jsonl",
            outside.as_ref(),
            "",
        ] {
            wire.do_get(ticket.as_bytes(), true).await;
        }
        wire.do_get(b"\xff", true).await;

        wire.do_put(false).await;
        wire.do_put(true).await;

        wire.do_exchange_path("data.jsonl", false).await;
        wire.do_exchange_path("data.jsonl", true).await;
        wire.do_exchange("no message", Vec::new(), true).await;

        wire.do_action(CAPABILITY_ACTION, "", true, false).await;
        wire.do_action(CAPABILITY_ACTION, "", true, true).await;
        wire.do_action("unknown", "", true, true).await;
        wire.list_actions(false).await;
        wire.list_actions(true).await;

        assert_eq!(wire.seen, EXPECTED, "\n<<<WIRE\n{}WIRE>>>", wire.seen);
    }
}

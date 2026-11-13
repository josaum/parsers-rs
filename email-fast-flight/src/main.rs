//! Apache Arrow Flight server for email parsing
//!
//! This server provides a gRPC interface for querying email data using
//! Apache Arrow Flight protocol.
//!
//! ## Quick Start
//!
//! ```bash
//! # Start the server
//! email-fast-flight --root /path/to/emails --port 50054
//!
//! # Query with any Arrow Flight client
//! ```

use anyhow::Result;
use arrow_flight::encode::FlightDataEncoderBuilder;
use arrow_flight::error::FlightError;
use arrow_flight::{
    flight_descriptor::DescriptorType,
    flight_service_server::{FlightService, FlightServiceServer},
    Action, ActionType, Criteria, Empty, FlightData, FlightDescriptor, FlightEndpoint, FlightInfo,
    HandshakeRequest, HandshakeResponse, PollInfo, PutResult, SchemaAsIpc, SchemaResult, Ticket,
};
use clap::Parser;
use email_fast_core::arrow::{emails_schema, emails_to_record_batch};
use email_fast_core::{parse_email, ParseOptions};
use flight_ipc_config::{
    capability_json, do_put_unsupported, parse_writer_policies_for_process, FlightIpcDirection,
    FlightIpcObservationGuard, FlightRequestAuthVerifier, FlightRpcAction, ParserRoute,
    CAPABILITY_ACTION,
};
use futures::StreamExt;
use futures::{stream, Stream};
use std::fs;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use tokio::net::TcpListener;
use tokio_stream::wrappers::TcpListenerStream;
use tonic::codegen::Bytes;
use tonic::{transport::Server, Request, Response, Status, Streaming};
use tracing::info;

#[cfg(test)]
#[path = "../../flight-ipc-config/test-support/mod.rs"]
mod flight_auth_test_support;

/// Default listen port. Distinct from xlsx-fast-flight (50051).
const DEFAULT_EMAIL_FLIGHT_PORT: u16 = 50054;

#[derive(Parser, Debug)]
#[command(name = "email-fast-flight")]
#[command(about = "Apache Arrow Flight server for email parsing")]
struct Args {
    /// Root directory containing email files
    #[arg(short, long)]
    root: PathBuf,

    /// Port to listen on.
    ///
    /// Default 50054 so this process does not collide with xlsx-fast-flight
    /// (50051). docx=50052, pptx=50053, xsd=50055.
    #[arg(short, long, default_value_t = DEFAULT_EMAIL_FLIGHT_PORT)]
    port: u16,

    /// Host to bind to
    #[arg(short = 'H', long, default_value = "0.0.0.0")]
    host: String,

    /// Max attachment size in MB
    #[arg(long, default_value = "100")]
    max_attachment_size: usize,
}

struct EmailFlightService {
    root: PathBuf,
    options: ParseOptions,
    /// Serialized once at startup; DoAction must not reserialize on every call.
    capability_body: Vec<u8>,
    auth_verifier: FlightRequestAuthVerifier,
}

impl EmailFlightService {
    fn new(
        root: PathBuf,
        options: ParseOptions,
        auth_verifier: FlightRequestAuthVerifier,
    ) -> Result<Self> {
        let capability_body = serde_json::to_vec(&capability_json(ParserRoute::Email, None)?)?;
        Ok(Self {
            root,
            options,
            capability_body,
            auth_verifier,
        })
    }

    fn authorize<T>(&self, request: &Request<T>) -> Result<(), Status> {
        self.auth_verifier.verify_metadata(request.metadata())
    }

    fn resolve_path(&self, path: &str) -> Result<PathBuf, Status> {
        let full_path = self.root.join(path);

        // Security: ensure path is within root
        let canonical = full_path
            .canonicalize()
            .map_err(|e| Status::invalid_argument(format!("Invalid path: {}", e)))?;
        let root_canonical = self
            .root
            .canonicalize()
            .map_err(|e| Status::internal(format!("Root path error: {}", e)))?;

        if !canonical.starts_with(&root_canonical) {
            return Err(Status::permission_denied("Path outside root directory"));
        }

        Ok(canonical)
    }

    fn list_email_files(&self, dir: &Path) -> Result<Vec<PathBuf>, Status> {
        let mut files = Vec::new();

        for entry in fs::read_dir(dir)
            .map_err(|e| Status::internal(format!("Failed to read directory: {}", e)))?
        {
            let entry =
                entry.map_err(|e| Status::internal(format!("Failed to read entry: {}", e)))?;
            let path = entry.path();
            // Skip symlinks — matches sibling *-flight servers so listing cannot
            // escape the root via a link that canonicalize would otherwise follow.
            let file_type = match entry.file_type() {
                Ok(ft) => ft,
                Err(_) => continue,
            };
            if file_type.is_symlink() {
                continue;
            }

            if file_type.is_dir() {
                files.extend(self.list_email_files(&path)?);
            } else if file_type.is_file() {
                if let Some(ext) = path.extension() {
                    let ext = ext.to_string_lossy().to_lowercase();
                    if matches!(ext.as_str(), "eml" | "msg" | "mbox") {
                        files.push(path);
                    }
                }
            }
        }

        Ok(files)
    }
}

#[tonic::async_trait]
impl FlightService for EmailFlightService {
    type HandshakeStream = Pin<Box<dyn Stream<Item = Result<HandshakeResponse, Status>> + Send>>;
    type ListFlightsStream = Pin<Box<dyn Stream<Item = Result<FlightInfo, Status>> + Send>>;
    type DoGetStream = Pin<Box<dyn Stream<Item = Result<FlightData, Status>> + Send>>;
    type DoPutStream = Pin<Box<dyn Stream<Item = Result<PutResult, Status>> + Send>>;
    type DoExchangeStream = Pin<Box<dyn Stream<Item = Result<FlightData, Status>> + Send>>;
    type ListActionsStream = Pin<Box<dyn Stream<Item = Result<ActionType, Status>> + Send>>;
    type DoActionStream = Pin<Box<dyn Stream<Item = Result<arrow_flight::Result, Status>> + Send>>;

    async fn poll_flight_info(
        &self,
        _request: Request<FlightDescriptor>,
    ) -> Result<Response<PollInfo>, Status> {
        Err(Status::unimplemented(POLL_FLIGHT_INFO_UNSUPPORTED))
    }

    async fn handshake(
        &self,
        _request: Request<Streaming<HandshakeRequest>>,
    ) -> Result<Response<Self::HandshakeStream>, Status> {
        // Parity with docx/pptx/xlsx/xsd: accept Handshake with an empty
        // success stream (no auth). Hard-failing here breaks clients that
        // always handshake before ListFlights/DoGet.
        Ok(Response::new(empty_handshake_stream()))
    }

    async fn list_flights(
        &self,
        request: Request<Criteria>,
    ) -> Result<Response<Self::ListFlightsStream>, Status> {
        self.authorize(&request)?;
        let files = self.list_email_files(&self.root)?;
        let root = self.root.clone();

        let stream = stream::iter(files.into_iter().map(move |path| {
            let rel = path.strip_prefix(&root).unwrap_or(&path);
            let components: Vec<String> = rel
                .components()
                .map(|c| c.as_os_str().to_string_lossy().to_string())
                .filter(|s| !s.is_empty())
                .collect();

            let descriptor = FlightDescriptor {
                r#type: DescriptorType::Path as i32,
                cmd: Bytes::new(),
                path: components,
            };

            let endpoint = FlightEndpoint {
                ticket: Some(Ticket {
                    ticket: Bytes::new(),
                }),
                location: vec![],
                expiration_time: None,
                app_metadata: Bytes::new(),
            };

            Ok(FlightInfo {
                schema: Bytes::new(),
                flight_descriptor: Some(descriptor),
                endpoint: vec![endpoint],
                total_records: -1,
                total_bytes: -1,
                ordered: false,
                app_metadata: Bytes::new(),
            })
        }));

        Ok(Response::new(Box::pin(stream)))
    }

    async fn get_flight_info(
        &self,
        request: Request<FlightDescriptor>,
    ) -> Result<Response<FlightInfo>, Status> {
        self.authorize(&request)?;
        let descriptor = request.into_inner();
        let path = descriptor.path.join("/");

        let full_path = self.resolve_path(&path)?;

        if !full_path.exists() {
            return Err(Status::not_found(format!("File not found: {}", path)));
        }

        let endpoint = FlightEndpoint {
            ticket: Some(Ticket {
                ticket: Bytes::from(path.as_bytes().to_vec()),
            }),
            location: vec![],
            expiration_time: None,
            app_metadata: Bytes::new(),
        };

        let schema = emails_schema();
        let message: arrow_flight::IpcMessage = SchemaAsIpc::new(&schema, &Default::default())
            .try_into()
            .map_err(|e| Status::internal(format!("Schema error: {:?}", e)))?;

        Ok(Response::new(FlightInfo {
            schema: message.0,
            flight_descriptor: Some(descriptor),
            endpoint: vec![endpoint],
            total_records: -1,
            total_bytes: -1,
            ordered: false,
            app_metadata: Bytes::new(),
        }))
    }

    async fn get_schema(
        &self,
        request: Request<FlightDescriptor>,
    ) -> Result<Response<SchemaResult>, Status> {
        self.authorize(&request)?;
        let schema = emails_schema();
        let message: arrow_flight::IpcMessage = SchemaAsIpc::new(&schema, &Default::default())
            .try_into()
            .map_err(|e| Status::internal(format!("Schema error: {:?}", e)))?;

        Ok(Response::new(SchemaResult { schema: message.0 }))
    }

    async fn do_get(
        &self,
        request: Request<Ticket>,
    ) -> Result<Response<Self::DoGetStream>, Status> {
        self.authorize(&request)?;
        let observation = FlightIpcObservationGuard::new(
            ParserRoute::Email,
            FlightIpcDirection::Read,
            FlightRpcAction::DoGet,
        );
        let ticket = request.into_inner();
        let path = String::from_utf8(ticket.ticket.to_vec())
            .map_err(|e| Status::invalid_argument(format!("Invalid ticket: {}", e)))?;

        let full_path = self.resolve_path(&path)?;

        let email = parse_email(&full_path, self.options)
            .map_err(|e| Status::internal(format!("Failed to parse email: {}", e)))?;
        let batch = emails_to_record_batch(std::slice::from_ref(&email))
            .map_err(|e| Status::internal(format!("Arrow encode: {}", e)))?;
        let schema = std::sync::Arc::new(emails_schema());
        let batch_stream = stream::iter(vec![Ok::<_, FlightError>(batch)]);
        let flight_stream = FlightDataEncoderBuilder::new()
            .with_schema(schema)
            .build(batch_stream)
            .map(|res| res.map_err(|err| Status::internal(err.to_string())));

        let stream = flight_ipc_config::observe_flight_stream(flight_stream, observation);
        Ok(Response::new(Box::pin(stream)))
    }

    async fn do_put(
        &self,
        _request: Request<Streaming<FlightData>>,
    ) -> Result<Response<Self::DoPutStream>, Status> {
        Err(do_put_unsupported())
    }

    async fn do_exchange(
        &self,
        _request: Request<Streaming<FlightData>>,
    ) -> Result<Response<Self::DoExchangeStream>, Status> {
        Err(Status::unimplemented("DoExchange not implemented"))
    }

    async fn do_action(
        &self,
        request: Request<Action>,
    ) -> Result<Response<Self::DoActionStream>, Status> {
        self.authorize(&request)?;
        let action = request.into_inner();
        if action.r#type != CAPABILITY_ACTION {
            return Err(Status::unimplemented("DoAction not implemented"));
        }
        let body = self.capability_body.clone();
        Ok(Response::new(Box::pin(stream::once(async move {
            Ok(arrow_flight::Result { body: body.into() })
        }))))
    }

    async fn list_actions(
        &self,
        request: Request<Empty>,
    ) -> Result<Response<Self::ListActionsStream>, Status> {
        self.authorize(&request)?;
        let action = ActionType {
            r#type: CAPABILITY_ACTION.to_owned(),
            description: "cached codec capabilities".to_owned(),
        };
        let stream = stream::once(async move { Ok(action) });
        Ok(Response::new(Box::pin(stream)))
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt::init();

    let args = Args::parse();
    let auth_verifier = FlightRequestAuthVerifier::from_env_required()?;

    // Validate both process-level values before binding.  Email has no record
    // batch writer route, so any explicit non-`none` value is unsafe.
    parse_writer_policies_for_process(
        std::env::var("PARSERS_FLIGHT_IPC_COMPRESSION")
            .ok()
            .as_deref(),
        std::env::var("PARSERS_FLIGHT_IPC_COMPRESSION_OVERRIDES")
            .ok()
            .as_deref(),
        ParserRoute::Email,
    )?;

    let addr: SocketAddr = format!("{}:{}", args.host, args.port).parse()?;

    // `max_total_attachment_bytes` arrived with the sovereign parsers-rs release.
    // Upstream defaults it equal to the per-attachment cap; keep that relationship
    // against the operator's flag, so raising --max-attachment-size is not silently
    // undone by a 100 MB total budget the operator never chose.
    let options = ParseOptions {
        max_inflate: 128 * 1024 * 1024,
        max_attachment_size: args.max_attachment_size * 1024 * 1024,
        max_total_attachment_bytes: args.max_attachment_size * 1024 * 1024,
        include_raw_headers: false,
        html_to_text: false,
        extract_attachment_content: false,
    };

    let service = EmailFlightService::new(args.root.clone(), options, auth_verifier)?;

    info!("Starting email-fast-flight server on {}", addr);
    info!("Root directory: {}", args.root.display());

    let listener = TcpListener::bind(addr).await?;
    let stream = TcpListenerStream::new(listener);

    Server::builder()
        .add_service(FlightServiceServer::new(service))
        .serve_with_incoming(stream)
        .await?;

    Ok(())
}

const POLL_FLIGHT_INFO_UNSUPPORTED: &str = "poll_flight_info";

fn empty_handshake_stream() -> Pin<Box<dyn Stream<Item = Result<HandshakeResponse, Status>> + Send>>
{
    Box::pin(stream::empty())
}

#[cfg(test)]
mod symlink_listing_tests {
    use super::*;
    #[cfg(unix)]
    use std::os::unix::fs::symlink as symlink_file;
    #[cfg(windows)]
    use std::os::windows::fs::symlink_file;

    #[test]
    fn list_email_files_skips_symlinks() {
        // WHY: listing must not follow symlinks that could escape the Flight
        // root; canonicalize on DoGet is not enough if ListFlights advertises
        // the linked path.
        let root = tempfile::tempdir().expect("tempdir");
        let outside = tempfile::tempdir().expect("outside");
        let real = outside.path().join("escape.eml");
        std::fs::write(&real, b"From: a@b\n\n").expect("write");
        let link = root.path().join("escape.eml");
        if let Err(err) = symlink_file(&real, &link) {
            #[cfg(windows)]
            if err.raw_os_error() == Some(1314) {
                return;
            }
            panic!("symlink: {err}");
        }
        let inside = root.path().join("ok.eml");
        std::fs::write(&inside, b"From: a@b\n\n").expect("write inside");

        let svc = EmailFlightService::new(
            root.path().to_path_buf(),
            ParseOptions::default(),
            flight_auth_test_support::required_auth_verifier(),
        )
        .expect("test service");
        let found = svc.list_email_files(root.path()).expect("list");
        assert_eq!(found.len(), 1);
        assert_eq!(found[0], inside);
    }
}

#[cfg(test)]
mod read_only_flight_contract_tests {
    use super::*;
    use crate::flight_auth_test_support::{required_auth_verifier, with_valid_bearer};
    use futures::StreamExt;

    fn capability_test_service() -> EmailFlightService {
        EmailFlightService::new(
            PathBuf::new(),
            ParseOptions::default(),
            required_auth_verifier(),
        )
        .expect("test service")
    }

    #[tokio::test]
    async fn handshake_returns_empty_ok_stream() {
        // WHY: email used to hard-fail Handshake while sibling *-flight
        // servers return an empty success stream. Clients that always
        // handshake before ListFlights must not get Unimplemented here.
        let mut stream = empty_handshake_stream();
        assert!(stream.next().await.is_none());
    }

    #[test]
    fn default_port_does_not_collide_with_xlsx() {
        // WHY: both servers used 50051; bind order then silently stole the port.
        assert_eq!(DEFAULT_EMAIL_FLIGHT_PORT, 50054);
        assert_ne!(DEFAULT_EMAIL_FLIGHT_PORT, 50051);
    }

    #[test]
    fn emails_schema_is_non_empty() {
        // WHY: get_schema used to advertise an empty Arrow schema while
        // do_get stuffed JSON into FlightData.data_body.
        let schema = emails_schema();
        assert!(schema.fields().len() >= 8);
        assert_eq!(schema.field(0).name(), "message_id");
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
                "__missing_auth_probe__.eml".to_owned(),
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
            "__missing_auth_probe__.eml".to_owned(),
        ])));
        let Err(error) = FlightService::get_flight_info(&service, request).await else {
            panic!("missing test file must reach path validation");
        };
        assert_eq!(error.code(), tonic::Code::InvalidArgument);
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
    use crate::flight_wire_test_support::Wire;

    const EXPECTED: &str = include_str!("../tests/wire_transcript.txt");

    const FIXTURE: &[u8] = b"From: Ada <ada@example.com>\r\n\
To: Bob <bob@example.com>\r\n\
Subject: Wire test\r\n\
Date: Mon, 01 Jan 2024 10:00:00 +0000\r\n\
Message-ID: <wire-1@example.com>\r\n\
MIME-Version: 1.0\r\n\
Content-Type: text/plain; charset=utf-8\r\n\
\r\n\
Hello from the wire test.\r\n";

    #[tokio::test]
    async fn every_rpc_answers_as_it_did_before_the_shared_server() {
        let base = tempfile::tempdir().expect("tempdir");
        let root = base.path().join("root");
        std::fs::create_dir_all(root.join("sub")).expect("directories");
        std::fs::write(root.join("mail.eml"), FIXTURE).expect("fixture");
        std::fs::write(root.join("sub").join("Upper.EML"), FIXTURE).expect("nested");
        std::fs::write(root.join("notes.txt"), b"not a message").expect("other file");
        std::fs::write(base.path().join("outside.eml"), FIXTURE).expect("outside");
        std::os::unix::fs::symlink(base.path().join("outside.eml"), root.join("link.eml"))
            .expect("symlink");
        let outside = base.path().join("outside.eml");
        let outside = outside.to_string_lossy();

        let service = EmailFlightService::new(
            root.clone(),
            ParseOptions::default(),
            required_auth_verifier(),
        )
        .expect("service");
        let mut wire = Wire::start(service, base.path()).await;

        wire.handshake(false).await;
        wire.handshake(true).await;
        wire.list_flights(false).await;
        wire.list_flights(true).await;

        wire.get_flight_info("mail.eml", false).await;
        for path in [
            "mail.eml",
            "mail.eml?batch=1",
            "sub/Upper.EML",
            "sub\\Upper.EML",
            "notes.txt",
            "sub",
            "missing.eml",
            "../outside.eml",
            "link.eml",
            outside.as_ref(),
            "",
        ] {
            wire.get_flight_info(path, true).await;
        }
        wire.get_flight_info_parts(&["sub", "Upper.EML"], true)
            .await;
        wire.get_flight_info_parts(&["sub", "..", "mail.eml"], true)
            .await;
        wire.get_flight_info_cmd(b"mail.eml", true).await;
        wire.get_flight_info_cmd(b"", true).await;

        wire.poll_flight_info("mail.eml", false).await;
        wire.poll_flight_info("mail.eml", true).await;

        wire.get_schema("mail.eml", false).await;
        wire.get_schema("mail.eml", true).await;
        wire.get_schema("missing.eml", true).await;

        wire.do_get(b"mail.eml", false).await;
        for ticket in [
            "mail.eml",
            "mail.eml?batch=1",
            "sub/Upper.EML",
            "sub\\Upper.EML",
            "notes.txt",
            "sub",
            "missing.eml",
            "../outside.eml",
            "sub/../mail.eml",
            "link.eml",
            outside.as_ref(),
            "",
        ] {
            wire.do_get(ticket.as_bytes(), true).await;
        }
        wire.do_get(b"\xff", true).await;

        wire.do_put(false).await;
        wire.do_put(true).await;

        wire.do_exchange_path("mail.eml", false).await;
        wire.do_exchange_path("mail.eml", true).await;
        wire.do_exchange("no message", Vec::new(), true).await;

        wire.do_action_lengths(CAPABILITY_ACTION, "", false).await;
        wire.do_action_lengths(CAPABILITY_ACTION, "", true).await;
        wire.do_action("unknown", "", true, true).await;
        wire.list_actions(false).await;
        wire.list_actions(true).await;

        assert_eq!(wire.seen, EXPECTED, "\n<<<WIRE\n{}WIRE>>>", wire.seen);
    }
}
